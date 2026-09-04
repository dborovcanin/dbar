//! Asking the wireless stack what network an interface is on.
//!
//! The name of a network is not in `/sys`. The kernel keeps it in nl80211, which is a
//! generic netlink family, so getting at it means three small things: find the family by
//! name, ask it to describe every wireless interface, and read the name out of the reply.
//!
//! This is a hundred lines of message building rather than a dependency because that is all
//! it is. Nothing here blocks for long - the reply is already in the kernel's hands when
//! the request returns - so it happens on the collector's own tick like any other read.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

use anyhow::{Context as _, Result, bail};

/// The control family, which every generic netlink socket starts by talking to.
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

const NL80211_CMD_GET_INTERFACE: u8 = 5;
const NL80211_ATTR_IFINDEX: u16 = 3;
const NL80211_ATTR_SSID: u16 = 52;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
/// `NLM_F_ROOT | NLM_F_MATCH`: answer for everything, not just one.
const NLM_F_DUMP: u16 = 0x300;

const HEADER: usize = 16;
const GENL_HEADER: usize = 4;

/// A connection to the wireless stack, kept open because a module asks again every tick.
pub struct Wireless {
    socket: OwnedFd,
    family: u16,
    sequence: u32,
}

impl Wireless {
    /// Find nl80211, or say why not: a machine with no wireless has no family to find.
    pub fn open() -> Result<Wireless> {
        let socket = open_socket()?;
        let mut wireless = Wireless {
            socket,
            family: GENL_ID_CTRL,
            sequence: 0,
        };
        wireless.family = wireless.resolve_family()?;
        Ok(wireless)
    }

    /// The network this interface is on, if it is on one.
    ///
    /// A card that is up but associated with nothing has no name to give, which is an
    /// answer rather than a failure.
    pub fn network_of(&mut self, ifindex: u32) -> Result<Option<String>> {
        let mut request = Vec::new();
        request.extend_from_slice(&genl_header(NL80211_CMD_GET_INTERFACE, 0));
        self.send(self.family, NLM_F_REQUEST | NLM_F_DUMP, &request)?;

        let mut found = None;
        self.receive(true, |payload| {
            let mut ifindex_here = None;
            let mut ssid = None;
            for (kind, value) in attributes(&payload[GENL_HEADER..]) {
                match kind {
                    NL80211_ATTR_IFINDEX if value.len() >= 4 => {
                        ifindex_here =
                            Some(u32::from_ne_bytes([value[0], value[1], value[2], value[3]]));
                    }
                    // An SSID is bytes rather than text: it is whatever the access point
                    // was named, and nothing promises that is UTF-8.
                    NL80211_ATTR_SSID => ssid = Some(String::from_utf8_lossy(value).into_owned()),
                    _ => {}
                }
            }
            if ifindex_here == Some(ifindex)
                && let Some(ssid) = ssid
            {
                found = Some(ssid);
            }
        })?;
        Ok(found)
    }

    /// The number nl80211 answers to on this machine, which is assigned at boot.
    fn resolve_family(&mut self) -> Result<u16> {
        let mut request = Vec::new();
        request.extend_from_slice(&genl_header(CTRL_CMD_GETFAMILY, 1));
        request.extend_from_slice(&attribute(CTRL_ATTR_FAMILY_NAME, b"nl80211\0"));
        self.send(GENL_ID_CTRL, NLM_F_REQUEST, &request)?;

        let mut family = None;
        self.receive(false, |payload| {
            for (kind, value) in attributes(&payload[GENL_HEADER..]) {
                if kind == CTRL_ATTR_FAMILY_ID && value.len() >= 2 {
                    family = Some(u16::from_ne_bytes([value[0], value[1]]));
                }
            }
        })?;
        family.context("this kernel has no nl80211; is there any wireless hardware?")
    }

    fn send(&mut self, kind: u16, flags: u16, payload: &[u8]) -> Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut message = Vec::with_capacity(HEADER + payload.len());
        message.extend_from_slice(&((HEADER + payload.len()) as u32).to_ne_bytes());
        message.extend_from_slice(&kind.to_ne_bytes());
        message.extend_from_slice(&flags.to_ne_bytes());
        message.extend_from_slice(&self.sequence.to_ne_bytes());
        // Zero is this socket's own port, which the kernel filled in when it was bound.
        message.extend_from_slice(&0u32.to_ne_bytes());
        message.extend_from_slice(payload);

        // SAFETY: the buffer is owned here and outlives the call, and its length is its own.
        let sent = unsafe {
            libc::send(
                self.socket.as_raw_fd(),
                message.as_ptr().cast(),
                message.len(),
                0,
            )
        };
        if sent < 0 {
            return Err(std::io::Error::last_os_error()).context("asking nl80211");
        }
        Ok(())
    }

    /// Read replies, handing each one to `take`.
    ///
    /// A dump ends with the kernel saying so, and is read until it does. A plain request
    /// is answered with one message and nothing after it, so waiting for an end that is
    /// never sent would hang the collector - and with it the bar, since this is read on
    /// the same thread that draws.
    fn receive(&mut self, dump: bool, mut take: impl FnMut(&[u8])) -> Result<()> {
        let mut buffer = vec![0u8; 32768];
        loop {
            // SAFETY: the buffer is owned here and outlives the call, and its length is
            // the length of the allocation.
            let read = unsafe {
                libc::recv(
                    self.socket.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("reading nl80211's reply");
            }

            let mut rest = &buffer[..read as usize];
            while rest.len() >= HEADER {
                let length = u32::from_ne_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
                let kind = u16::from_ne_bytes([rest[4], rest[5]]);
                if length < HEADER || length > rest.len() {
                    bail!("nl80211 sent a message that does not fit its own length");
                }
                match kind {
                    NLMSG_DONE => return Ok(()),
                    NLMSG_ERROR => {
                        // The payload starts with the errno, negated. Zero is an ack.
                        let code = i32::from_ne_bytes([
                            rest[HEADER],
                            rest[HEADER + 1],
                            rest[HEADER + 2],
                            rest[HEADER + 3],
                        ]);
                        if code == 0 {
                            return Ok(());
                        }
                        return Err(std::io::Error::from_raw_os_error(-code))
                            .context("nl80211 refused the request");
                    }
                    _ => take(&rest[HEADER..length]),
                }
                // Messages are padded to four bytes, and the next one starts after that.
                let step = length.div_ceil(4) * 4;
                rest = &rest[step.min(rest.len())..];
            }
            if !dump || read == 0 {
                return Ok(());
            }
        }
    }
}

fn open_socket() -> Result<OwnedFd> {
    // SAFETY: a socket call with constant arguments, checked before it is owned.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_GENERIC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("opening a netlink socket");
    }
    // SAFETY: the descriptor is fresh, checked, and owned by nothing else.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };

    // SAFETY: sockaddr_nl is plain data, and zero is valid for every field of it.
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    // SAFETY: the address is a fully initialised sockaddr_nl and the length is its own.
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            std::ptr::addr_of!(address).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if bound < 0 {
        return Err(std::io::Error::last_os_error()).context("binding a netlink socket");
    }
    Ok(socket)
}

fn genl_header(command: u8, version: u8) -> [u8; GENL_HEADER] {
    [command, version, 0, 0]
}

/// One netlink attribute: a length, a type, and a padded payload.
fn attribute(kind: u16, value: &[u8]) -> Vec<u8> {
    let length = 4 + value.len();
    let mut out = Vec::with_capacity(length.div_ceil(4) * 4);
    out.extend_from_slice(&(length as u16).to_ne_bytes());
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(value);
    out.resize(length.div_ceil(4) * 4, 0);
    out
}

/// Walk a run of attributes, skipping anything that does not fit.
fn attributes(mut rest: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if rest.len() < 4 {
            return None;
        }
        let length = u16::from_ne_bytes([rest[0], rest[1]]) as usize;
        let kind = u16::from_ne_bytes([rest[2], rest[3]]);
        // A run that says an attribute is longer than what is left is truncated, and
        // there is nothing after it worth reading either.
        if length < 4 || length > rest.len() {
            return None;
        }
        let value = &rest[4..length];
        rest = &rest[(length.div_ceil(4) * 4).min(rest.len())..];
        Some((kind, value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attribute_is_padded_to_four_bytes() {
        // Seven bytes of name plus the terminator is eight, and the header four.
        let built = attribute(CTRL_ATTR_FAMILY_NAME, b"nl80211\0");
        assert_eq!(built.len(), 12);
        assert_eq!(u16::from_ne_bytes([built[0], built[1]]) as usize, 12);

        // Five bytes of payload become eight, so the next attribute starts aligned.
        let padded = attribute(1, b"12345");
        assert_eq!(padded.len(), 12);
        assert_eq!(u16::from_ne_bytes([padded[0], padded[1]]), 9);
    }

    #[test]
    fn attributes_are_walked_over_their_padding() {
        let mut run = Vec::new();
        run.extend_from_slice(&attribute(NL80211_ATTR_IFINDEX, &7u32.to_ne_bytes()));
        run.extend_from_slice(&attribute(NL80211_ATTR_SSID, b"a network"));

        let found: Vec<(u16, Vec<u8>)> = attributes(&run)
            .map(|(kind, value)| (kind, value.to_vec()))
            .collect();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, NL80211_ATTR_IFINDEX);
        assert_eq!(found[0].1, 7u32.to_ne_bytes());
        assert_eq!(found[1].0, NL80211_ATTR_SSID);
        assert_eq!(found[1].1, b"a network");
    }

    #[test]
    fn a_truncated_attribute_ends_the_walk_rather_than_reading_past_it() {
        let mut run = attribute(NL80211_ATTR_SSID, b"a network");
        run.truncate(6);
        assert_eq!(attributes(&run).count(), 0);
    }
}
