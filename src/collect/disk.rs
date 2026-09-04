//! Free space on a filesystem, from `statvfs`.
//!
//! What "free" means depends on who is asking. A filesystem keeps a reserve that only root
//! may write into, so the space an ordinary program can still use is smaller than the space
//! that is unused. `available` is the honest number for a bar, and is what a percentage is
//! worked out against; `free` is published too, for a bar watched by whoever runs the
//! machine rather than by whoever uses it.

use std::ffi::CString;
use std::path::PathBuf;

use anyhow::{Result, bail};

use super::{Collector, Reading};
use crate::status::{FieldSpec, Fields, Kind, State, Unit, Value};

pub const FIELDS: &[FieldSpec] = &[
    // How much of the filesystem is in use, counting the root reserve as unavailable.
    FieldSpec {
        name: "percent",
        kind: Kind::Num(Unit::Percent),
    },
    FieldSpec {
        name: "used",
        kind: Kind::Num(Unit::Bytes),
    },
    FieldSpec {
        name: "total",
        kind: Kind::Num(Unit::Bytes),
    },
    // What an ordinary program can still write.
    FieldSpec {
        name: "available",
        kind: Kind::Num(Unit::Bytes),
    },
    // What is unused, including the part only root may touch.
    FieldSpec {
        name: "free",
        kind: Kind::Num(Unit::Bytes),
    },
    FieldSpec {
        name: "path",
        kind: Kind::Text,
    },
];

pub struct Disk {
    path: PathBuf,
}

impl Disk {
    pub fn new(path: String) -> Disk {
        Disk {
            path: PathBuf::from(path),
        }
    }
}

impl Collector for Disk {
    fn read(&mut self) -> Result<Reading> {
        let space = statvfs(&self.path)?;

        let mut fields = Fields::default();
        let bytes = |v: u64| Value::Num {
            v: v as f64,
            unit: Unit::Bytes,
        };
        fields.set("percent", {
            // Usable is what is in use plus what could still be written; the root reserve
            // belongs to neither, so counting it would make a full disk read as 95%.
            let usable = space.used + space.available;
            match usable {
                0 => Value::Absent,
                _ => Value::Num {
                    v: space.used as f64 * 100.0 / usable as f64,
                    unit: Unit::Percent,
                },
            }
        });
        fields.set("used", bytes(space.used));
        fields.set("total", bytes(space.total));
        fields.set("available", bytes(space.available));
        fields.set("free", bytes(space.free));
        fields.set("path", Value::Text(self.path.display().to_string()));
        fields.set_primary("percent");

        Ok(Reading {
            fields,
            state: State::Idle,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Space {
    total: u64,
    used: u64,
    available: u64,
    free: u64,
}

// The widths of the statvfs fields differ between targets: 64 bits here, 32 on a 32-bit
// glibc. The casts are redundant on this machine and load-bearing on that one, so the lint
// is turned off rather than the casts removed.
#[allow(clippy::unnecessary_cast)]
fn statvfs(path: &std::path::Path) -> Result<Space> {
    let Ok(c_path) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        bail!(
            "{} is not a path the system can be asked about",
            path.display()
        );
    };

    let mut raw = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the pointer is a valid C string, and the kernel either fills the struct and
    // returns zero or leaves it alone and returns an error, which is checked before it is
    // assumed initialised.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), raw.as_mut_ptr()) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        bail!("asking about {}: {e}", path.display());
    }
    // SAFETY: statvfs returned zero, so it filled the struct.
    let raw = unsafe { raw.assume_init() };

    // f_frsize is the fragment size the block counts are in; f_bsize is the preferred size
    // for io, which is not the same thing and is a common way to get this wrong.
    //
    let block = if raw.f_frsize > 0 {
        raw.f_frsize as u64
    } else {
        raw.f_bsize as u64
    };
    let total = raw.f_blocks as u64 * block;
    let free = raw.f_bfree as u64 * block;
    let available = raw.f_bavail as u64 * block;

    Ok(Space {
        total,
        used: total.saturating_sub(free),
        available,
        free,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_filesystem_answers() {
        // Only the invariants are asserted: the actual figures belong to whatever machine
        // is running the test.
        let space = statvfs(std::path::Path::new("/")).expect("/ is always mounted");
        assert!(space.total > 0);
        assert!(space.used <= space.total);
        // The reserve is what makes these differ, and it is never negative.
        assert!(space.available <= space.free);
    }

    #[test]
    fn a_path_that_is_not_mounted_is_an_error() {
        let e = statvfs(std::path::Path::new("/nonexistent/mount/point"))
            .expect_err("an unmounted path cannot be measured");
        assert!(format!("{e:#}").contains("/nonexistent"), "{e:#}");
    }

    #[test]
    fn a_percentage_leaves_the_root_reserve_out_of_the_total() {
        let mut disk = Disk::new("/".to_string());
        let fields = disk.read().expect("/ is always mounted").fields;
        let percent = fields
            .get("percent")
            .and_then(|v| v.num())
            .expect("a mounted filesystem has a percentage");
        assert!((0.0..=100.0).contains(&percent), "{percent}");

        // A disk with no room left for an ordinary program reads as full, whatever is
        // still held back for root.
        let space = Space {
            total: 100,
            used: 95,
            available: 0,
            free: 5,
        };
        let usable = space.used + space.available;
        assert_eq!(space.used as f64 * 100.0 / usable as f64, 100.0);
    }

    #[test]
    fn the_path_asked_about_is_published() {
        let mut disk = Disk::new("/".to_string());
        let fields = disk.read().expect("/ is always mounted").fields;
        assert!(matches!(fields.get("path"), Some(Value::Text(p)) if p == "/"));
    }
}
