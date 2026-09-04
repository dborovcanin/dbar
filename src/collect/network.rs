//! Network throughput, from `/sys/class/net`.
//!
//! The kernel counts bytes since the interface came up, so a rate is a difference between
//! two readings over the time between them rather than something that can be sampled once.
//!
//! Picking an interface is most of the work. A machine running containers has dozens -
//! this one has thirty `veth` pairs, a bridge and a loopback - and none of them is what a
//! person means by "the network". Real hardware is the interfaces with a `device` symlink,
//! which is exactly the distinction, and among those the one that is actually up wins.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, bail};

use super::{Collector, Reading};
use crate::status::{FieldSpec, Fields, Kind, State, Unit, Value};

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "down",
        kind: Kind::Num(Unit::BytesPerSec),
    },
    FieldSpec {
        name: "up",
        kind: Kind::Num(Unit::BytesPerSec),
    },
    FieldSpec {
        name: "device",
        kind: Kind::Text,
    },
    // `up`, `down`, `dormant` - what the kernel says the link is doing.
    FieldSpec {
        name: "state",
        kind: Kind::Text,
    },
    // Everything sent and received since the interface came up.
    FieldSpec {
        name: "received",
        kind: Kind::Num(Unit::Bytes),
    },
    FieldSpec {
        name: "sent",
        kind: Kind::Num(Unit::Bytes),
    },
    // The wireless network this interface is on. Absent on a cable, and on a card that is
    // up but has joined nothing.
    FieldSpec {
        name: "ssid",
        kind: Kind::Text,
    },
];

const CLASS: &str = "/sys/class/net";

pub struct Network {
    class: PathBuf,
    /// The interface named in the config, or nothing to pick one.
    wanted: Option<String>,
    /// The last sample, to work a rate out against.
    previous: Option<Sample>,
    /// The wireless stack, opened the first time a wireless interface is read and kept
    /// afterwards. A machine with no wireless never opens it at all.
    wireless: Option<crate::collect::nl80211::Wireless>,
    /// Whether the wireless stack has already refused, so a machine without one is not
    /// asked on every tick.
    wireless_failed: bool,
}

impl Network {
    pub fn new(wanted: Option<String>) -> Network {
        Network {
            class: PathBuf::from(CLASS),
            wanted,
            previous: None,
            wireless: None,
            wireless_failed: false,
        }
    }
}

impl Network {
    /// The network this interface has joined, when it is a wireless one that has joined a
    /// network at all.
    ///
    /// A cable is never asked: `wireless` is the directory the kernel gives an interface
    /// that has a radio behind it, so its absence is the answer.
    fn network_on(&mut self, device: &Path) -> Option<String> {
        if !device.join("wireless").is_dir() || self.wireless_failed {
            return None;
        }
        if self.wireless.is_none() {
            match crate::collect::nl80211::Wireless::open() {
                Ok(wireless) => self.wireless = Some(wireless),
                Err(e) => {
                    log::debug!("no network name to report: {e:#}");
                    self.wireless_failed = true;
                    return None;
                }
            }
        }

        let ifindex = super::read_to_string(device.join("ifindex"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        match self.wireless.as_mut()?.network_of(ifindex) {
            Ok(ssid) => ssid,
            Err(e) => {
                // The socket is dropped rather than kept in a state nothing understands;
                // the next tick opens a new one.
                log::debug!("the network name could not be read: {e:#}");
                self.wireless = None;
                None
            }
        }
    }
}

impl Collector for Network {
    fn read(&mut self) -> Result<Reading> {
        let device = match &self.wanted {
            Some(name) => {
                let path = self.class.join(name);
                if !path.join("statistics").is_dir() {
                    bail!("there is no network interface called {name:?}");
                }
                path
            }
            // Chosen again on every tick, so unplugging the cable moves the module to the
            // wireless card without dbar being restarted.
            None => match pick(&self.class) {
                Some(path) => path,
                None => {
                    // A machine with no hardware interface at all is unusual but not
                    // broken; the module simply has nothing to say.
                    let mut fields = Fields::default();
                    for name in ["down", "up", "device", "state", "received", "sent", "ssid"] {
                        fields.set(name, Value::Absent);
                    }
                    fields.set_primary("down");
                    return Ok(Reading {
                        fields,
                        state: State::Idle,
                    });
                }
            },
        };

        let now = Sample::read(&device)?;
        // A different interface means the counters are not comparable, so the first tick
        // after a switch reports no rate rather than an enormous one.
        let previous = self
            .previous
            .replace(now.clone())
            .filter(|p| p.device == now.device);

        let mut fields = Fields::default();
        let rate = |bytes: Option<f64>| match bytes {
            Some(v) => Value::Num {
                v,
                unit: Unit::BytesPerSec,
            },
            None => Value::Absent,
        };
        fields.set(
            "down",
            rate(previous.as_ref().and_then(|p| now.rate(p, true))),
        );
        fields.set(
            "up",
            rate(previous.as_ref().and_then(|p| now.rate(p, false))),
        );
        fields.set("device", Value::Text(now.device.clone()));
        fields.set("state", Value::Text(now.state.clone()));
        fields.set(
            "ssid",
            match self.network_on(&device) {
                Some(ssid) => Value::Text(ssid),
                None => Value::Absent,
            },
        );
        fields.set(
            "received",
            Value::Num {
                v: now.received as f64,
                unit: Unit::Bytes,
            },
        );
        fields.set(
            "sent",
            Value::Num {
                v: now.sent as f64,
                unit: Unit::Bytes,
            },
        );
        fields.set_primary("down");

        Ok(Reading {
            fields,
            // A link that is down is worth saying so, but it is not an error: an unplugged
            // cable is a fact about the machine, not a failure to read it.
            state: match now.state.as_str() {
                "up" => State::Good,
                "down" => State::Warning,
                _ => State::Idle,
            },
        })
    }
}

#[derive(Clone, Debug)]
struct Sample {
    device: String,
    state: String,
    received: u64,
    sent: u64,
    at: Instant,
}

impl Sample {
    fn read(path: &Path) -> Result<Sample> {
        let stats = path.join("statistics");
        let counter = |name: &str| -> Result<u64> {
            let text = super::read_to_string(stats.join(name))?;
            text.trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("{name} of {} is not a number", path.display()))
        };
        Ok(Sample {
            device: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            state: super::read_to_string(path.join("operstate"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string()),
            received: counter("rx_bytes")?,
            sent: counter("tx_bytes")?,
            at: Instant::now(),
        })
    }

    /// Bytes a second since the previous sample.
    ///
    /// Nothing to report when the counters went backwards, which happens when an interface
    /// is brought down and up again, or when no time has passed.
    fn rate(&self, previous: &Sample, down: bool) -> Option<f64> {
        let seconds = self.at.duration_since(previous.at).as_secs_f64();
        if seconds <= 0.0 {
            return None;
        }
        let (now, before) = if down {
            (self.received, previous.received)
        } else {
            (self.sent, previous.sent)
        };
        Some(now.checked_sub(before)? as f64 / seconds)
    }
}

/// The interface to watch: real hardware, preferring one that is actually up.
fn pick(class: &Path) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(class) else {
        return None;
    };
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        // Only a physical interface has a device behind it. Loopback, bridges and the
        // `veth` pairs every container brings do not, which is what keeps a bar off them.
        .filter(|p| p.join("device").exists() && p.join("statistics").is_dir())
        .collect();

    candidates.sort_by_key(|p| {
        let state = super::read_to_string(p.join("operstate"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        // Up first, then by name so the choice does not depend on directory order.
        (
            state != "up",
            p.file_name().map(|n| n.to_os_string()).unwrap_or_default(),
        )
    });
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Build a `/sys/class/net`-shaped directory. An interface with `device` is hardware.
    fn class(name: &str, devices: &[(&str, bool, &str, u64, u64)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dbar-net-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for (device, hardware, state, rx, tx) in devices {
            let dir = root.join(device);
            std::fs::create_dir_all(dir.join("statistics")).expect("writable");
            if *hardware {
                std::fs::create_dir_all(dir.join("device")).expect("writable");
            }
            std::fs::write(dir.join("operstate"), format!("{state}\n")).expect("writable");
            std::fs::write(dir.join("statistics/rx_bytes"), format!("{rx}\n")).expect("writable");
            std::fs::write(dir.join("statistics/tx_bytes"), format!("{tx}\n")).expect("writable");
        }
        root
    }

    #[test]
    fn container_interfaces_are_not_the_network() {
        // What this laptop actually looks like: thirty veth pairs, a bridge, and one
        // wireless card that is the answer.
        let mut devices: Vec<(&str, bool, &str, u64, u64)> = vec![
            ("lo", false, "unknown", 1, 1),
            ("docker0", false, "up", 1, 1),
            ("veth17ba980", false, "up", 1, 1),
            ("veth1a436d0", false, "up", 1, 1),
        ];
        devices.push(("wlp3s0", true, "up", 100, 200));
        let root = class("containers", &devices);
        let picked = pick(&root).expect("the wireless card is hardware");
        assert_eq!(picked.file_name().unwrap(), "wlp3s0");
    }

    #[test]
    fn an_interface_that_is_up_beats_one_that_is_not() {
        let root = class(
            "prefer-up",
            &[
                // Sorts first by name, so only the link state can put the other ahead.
                ("enp2s0f0", true, "down", 0, 0),
                ("wlp3s0", true, "up", 100, 200),
            ],
        );
        assert_eq!(pick(&root).unwrap().file_name().unwrap(), "wlp3s0");
    }

    #[test]
    fn a_rate_needs_two_samples() {
        let root = class("rate", &[("wlp3s0", true, "up", 1000, 2000)]);
        let mut net = Network {
            class: root.clone(),
            wanted: Some("wlp3s0".to_string()),
            previous: None,
            wireless: None,
            wireless_failed: true,
        };

        // Nothing to divide by yet, so the first tick reports no rate rather than a wrong
        // one worked out against the interface's whole uptime.
        let first = net.read().expect("the fixture reads").fields;
        assert!(matches!(first.get("down"), Some(Value::Absent)));
        assert_eq!(first.get("received").and_then(|v| v.num()), Some(1000.0));

        // Two seconds later, a thousand more bytes in.
        let mut previous = net.previous.clone().expect("the first sample was kept");
        previous.at = Instant::now() - Duration::from_secs(2);
        previous.received = 0;
        previous.sent = 0;
        net.previous = Some(previous);

        let second = net.read().expect("the fixture reads").fields;
        let down = second.get("down").and_then(|v| v.num()).expect("a rate");
        assert!((down - 500.0).abs() < 1.0, "{down} should be about 500 B/s");
    }

    #[test]
    fn counters_going_backwards_report_nothing_rather_than_nonsense() {
        let now = Sample {
            device: "wlp3s0".to_string(),
            state: "up".to_string(),
            received: 100,
            sent: 100,
            at: Instant::now(),
        };
        let previous = Sample {
            received: 1000,
            at: now.at - Duration::from_secs(1),
            ..now.clone()
        };
        assert_eq!(now.rate(&previous, true), None);
    }

    #[test]
    fn naming_an_interface_that_is_not_there_is_reported() {
        let root = class("named", &[("wlp3s0", true, "up", 1, 1)]);
        let mut net = Network {
            class: root,
            wanted: Some("nonesuch".to_string()),
            previous: None,
            wireless: None,
            wireless_failed: true,
        };
        let e = net
            .read()
            .expect_err("an interface that is not there is a mistake");
        assert!(format!("{e:#}").contains("nonesuch"), "{e:#}");
    }

    #[test]
    fn a_machine_with_no_hardware_interface_reports_nothing() {
        let root = class("virtual-only", &[("lo", false, "unknown", 0, 0)]);
        let mut net = Network {
            class: root,
            wanted: None,
            previous: None,
            wireless: None,
            wireless_failed: true,
        };
        let fields = net.read().expect("this is not an error").fields;
        assert!(matches!(fields.get("device"), Some(Value::Absent)));
    }
}
