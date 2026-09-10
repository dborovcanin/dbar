//! Asking the wireless stack what network an interface is on.
//!
//! The name of a network is not in `/sys`. The kernel keeps it in nl80211's cached scan
//! results, while the associated access point and signal are station information. Reading
//! the station on every collector tick tells us when the association changes; the larger
//! scan-cache dump is needed only then, and never asks the radio to perform a scan.
//!
//! This is a small amount of message building rather than a dependency because that is all
//! it is. Nothing here blocks for long - the replies are already in the kernel's hands when
//! the requests return - so it happens on the collector's own tick like any other read.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

use anyhow::{Context as _, Result, bail};

/// The control family, which every generic netlink socket starts by talking to.
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

const NL80211_CMD_GET_STATION: u8 = 17;
const NL80211_CMD_GET_SCAN: u8 = 32;
const NL80211_ATTR_IFINDEX: u16 = 3;
const NL80211_ATTR_MAC: u16 = 6;
const NL80211_ATTR_STA_INFO: u16 = 21;
const NL80211_ATTR_BSS: u16 = 47;
/// Inside `STA_INFO`: the last signal, in dBm, as a signed byte.
const NL80211_STA_INFO_SIGNAL: u16 = 7;
/// Inside `BSS`: the raw information elements and association state.
const NL80211_BSS_INFORMATION_ELEMENTS: u16 = 6;
const NL80211_BSS_STATUS: u16 = 9;
const NL80211_BSS_BEACON_IES: u16 = 11;
const NL80211_BSS_STATUS_ASSOCIATED: u32 = 1;
const NL80211_BSS_STATUS_IBSS_JOINED: u32 = 2;

/// Netlink uses the high two type bits as flags, not as part of the attribute number.
const NLA_TYPE_MASK: u16 = 0x3fff;

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
    network: Option<CachedNetwork>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedNetwork {
    ifindex: u32,
    bssid: [u8; 6],
    ssid: String,
}

impl CachedNetwork {
    fn is_for(&self, ifindex: u32, bssid: [u8; 6]) -> bool {
        self.ifindex == ifindex && self.bssid == bssid
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Station {
    bssid: Option<[u8; 6]>,
    signal: Option<i8>,
}

impl Wireless {
    /// Find nl80211, or say why not: a machine with no wireless has no family to find.
    pub fn open() -> Result<Wireless> {
        let socket = open_socket()?;
        let mut wireless = Wireless {
            socket,
            family: GENL_ID_CTRL,
            sequence: 0,
            network: None,
        };
        wireless.family = wireless.resolve_family()?;
        Ok(wireless)
    }

    /// The network this interface is on and how well it can hear it.
    ///
    /// Station information is small and read on every tick. Its peer address is also the
    /// cache key for the SSID, so the larger scan-cache dump happens only on the first read
    /// of an association or after roaming/reconnecting. `GET_SCAN` reads results the kernel
    /// already has; unlike `TRIGGER_SCAN`, it causes no radio work.
    pub fn state_of(&mut self, ifindex: u32) -> Result<(Option<String>, Option<i8>)> {
        let Some(station) = self.station_of(ifindex)? else {
            self.network = None;
            return Ok((None, None));
        };

        let cached = station.bssid.and_then(|bssid| {
            self.network
                .as_ref()
                .filter(|network| network.is_for(ifindex, bssid))
                .map(|network| network.ssid.clone())
        });
        if let Some(ssid) = cached {
            return Ok((Some(ssid), station.signal));
        }

        let ssid = self.network_of(ifindex)?;
        self.network = match (station.bssid, ssid.as_ref()) {
            (Some(bssid), Some(ssid)) => Some(CachedNetwork {
                ifindex,
                bssid,
                ssid: ssid.clone(),
            }),
            _ => None,
        };
        Ok((ssid, station.signal))
    }

    /// The SSID of the BSS this interface is associated with, from cached scan results.
    fn network_of(&mut self, ifindex: u32) -> Result<Option<String>> {
        let mut request = Vec::new();
        request.extend_from_slice(&genl_header(NL80211_CMD_GET_SCAN, 0));
        request.extend_from_slice(&attribute(NL80211_ATTR_IFINDEX, &ifindex.to_ne_bytes()));
        self.send(self.family, NLM_F_REQUEST | NLM_F_DUMP, &request)?;

        let mut found = None;
        self.receive(true, |payload| {
            if let Some(ssid) = associated_ssid(payload) {
                found = Some(ssid);
            }
        })?;
        Ok(found)
    }

    /// The associated access point and how strong its link is, in dBm.
    ///
    /// The station is the access point this card is talking to; a card that has joined
    /// nothing has no station and no strength, which is an answer rather than a failure.
    fn station_of(&mut self, ifindex: u32) -> Result<Option<Station>> {
        let mut request = Vec::new();
        request.extend_from_slice(&genl_header(NL80211_CMD_GET_STATION, 0));
        request.extend_from_slice(&attribute(NL80211_ATTR_IFINDEX, &ifindex.to_ne_bytes()));
        self.send(self.family, NLM_F_REQUEST | NLM_F_DUMP, &request)?;

        let mut found = None;
        self.receive(true, |payload| {
            if let Some(station) = station(payload) {
                found = Some(station);
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
            for (kind, value) in attributes(genl_body(payload)) {
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
                let body = &rest[HEADER..length];
                match kind {
                    NLMSG_DONE => return Ok(()),
                    NLMSG_ERROR => {
                        // The payload starts with the errno, negated. Zero is an ack.
                        //
                        // Checked rather than assumed: this is bytes off a socket, and a
                        // message that says it holds an error without holding one would
                        // otherwise be read past its own end.
                        let Some(code) = body.get(..4) else {
                            bail!("nl80211 reported an error without saying what it was");
                        };
                        let code = i32::from_ne_bytes([code[0], code[1], code[2], code[3]]);
                        if code == 0 {
                            return Ok(());
                        }
                        return Err(std::io::Error::from_raw_os_error(-code))
                            .context("nl80211 refused the request");
                    }
                    _ => take(body),
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
/// What follows the generic-netlink header in a message, which is where its attributes are.
///
/// A message shorter than its own header holds nothing rather than being read past its end:
/// this is bytes off a socket, and slicing what is not there would take the bar down over a
/// reply that only the kernel could have got wrong.
fn genl_body(payload: &[u8]) -> &[u8] {
    payload.get(GENL_HEADER..).unwrap_or_default()
}

/// Read the associated peer and signal from one `GET_STATION` reply.
fn station(payload: &[u8]) -> Option<Station> {
    let mut station = Station::default();
    let mut present = false;
    for (kind, value) in attributes(genl_body(payload)) {
        match kind {
            NL80211_ATTR_MAC if value.len() >= 6 => {
                station.bssid = value[..6].try_into().ok();
                present = true;
            }
            NL80211_ATTR_STA_INFO => {
                // The station's details are a run of attributes of their own.
                for (inner, bytes) in attributes(value) {
                    if inner == NL80211_STA_INFO_SIGNAL && !bytes.is_empty() {
                        station.signal = Some(bytes[0] as i8);
                        present = true;
                    }
                }
            }
            _ => {}
        }
    }
    present.then_some(station)
}

/// Read the SSID from a scan reply only when it describes the current association.
fn associated_ssid(payload: &[u8]) -> Option<String> {
    for (kind, value) in attributes(genl_body(payload)) {
        if kind != NL80211_ATTR_BSS {
            continue;
        }

        let mut status = None;
        let mut information = None;
        let mut beacon = None;
        for (inner, bytes) in attributes(value) {
            match inner {
                NL80211_BSS_STATUS if bytes.len() >= 4 => {
                    status = Some(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
                }
                NL80211_BSS_INFORMATION_ELEMENTS => information = Some(bytes),
                NL80211_BSS_BEACON_IES => beacon = Some(bytes),
                _ => {}
            }
        }
        if !matches!(
            status,
            Some(NL80211_BSS_STATUS_ASSOCIATED | NL80211_BSS_STATUS_IBSS_JOINED)
        ) {
            continue;
        }

        // Probe-response IEs are preferred, with beacon IEs as a fallback. An SSID is
        // bytes rather than text, and nothing promises that it is UTF-8.
        if let Some(ssid) = information
            .and_then(ssid_from_ies)
            .or_else(|| beacon.and_then(ssid_from_ies))
        {
            return Some(ssid);
        }
    }
    None
}

/// Information element zero is the SSID: an id byte, a length byte, then its bytes.
fn ssid_from_ies(mut rest: &[u8]) -> Option<String> {
    while rest.len() >= 2 {
        let kind = rest[0];
        let length = rest[1] as usize;
        rest = &rest[2..];
        if length > rest.len() {
            return None;
        }
        let value = &rest[..length];
        if kind == 0 {
            return Some(String::from_utf8_lossy(value).into_owned());
        }
        rest = &rest[length..];
    }
    None
}

fn attributes(mut rest: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if rest.len() < 4 {
            return None;
        }
        let length = u16::from_ne_bytes([rest[0], rest[1]]) as usize;
        let kind = u16::from_ne_bytes([rest[2], rest[3]]) & NLA_TYPE_MASK;
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

    /// Everything here is bytes off a socket, and the two places that read past a length
    /// check were the two the kernel would have to get wrong for the bar to go down with
    /// it. A truncated message is nothing to report, not something to crash over.
    #[test]
    fn a_message_shorter_than_its_own_header_holds_nothing() {
        assert!(genl_body(&[]).is_empty());
        assert!(genl_body(&[1, 2, 3]).is_empty());
        assert!(genl_body(&[1, 2, 3, 4]).is_empty());
        assert_eq!(genl_body(&[1, 2, 3, 4, 9, 9]), &[9, 9]);
        // And what it hands over is walked without reading past it either.
        assert_eq!(attributes(genl_body(&[0, 0, 0, 0, 1])).count(), 0);
    }

    #[test]
    fn attributes_are_walked_over_their_padding() {
        let mut run = Vec::new();
        run.extend_from_slice(&attribute(NL80211_ATTR_IFINDEX, &7u32.to_ne_bytes()));
        run.extend_from_slice(&attribute(NL80211_ATTR_MAC, b"123456"));

        let found: Vec<(u16, Vec<u8>)> = attributes(&run)
            .map(|(kind, value)| (kind, value.to_vec()))
            .collect();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, NL80211_ATTR_IFINDEX);
        assert_eq!(found[0].1, 7u32.to_ne_bytes());
        assert_eq!(found[1].0, NL80211_ATTR_MAC);
        assert_eq!(found[1].1, b"123456");
    }

    #[test]
    fn a_truncated_attribute_ends_the_walk_rather_than_reading_past_it() {
        let mut run = attribute(NL80211_ATTR_MAC, b"123456");
        run.truncate(6);
        assert_eq!(attributes(&run).count(), 0);
    }

    #[test]
    fn attribute_flags_are_not_part_of_the_type() {
        let nested = attribute(NL80211_ATTR_BSS | 0x8000, b"bss");
        assert_eq!(
            attributes(&nested).next(),
            Some((NL80211_ATTR_BSS, &b"bss"[..]))
        );
    }

    #[test]
    fn a_station_reply_carries_the_cache_key_and_signal() {
        let mut details = Vec::new();
        details.extend_from_slice(&attribute(NL80211_STA_INFO_SIGNAL, &[(-45i8) as u8]));

        let mut reply = genl_header(NL80211_CMD_GET_STATION, 0).to_vec();
        reply.extend_from_slice(&attribute(NL80211_ATTR_MAC, &[1, 2, 3, 4, 5, 6]));
        reply.extend_from_slice(&attribute(NL80211_ATTR_STA_INFO | 0x8000, &details));

        assert_eq!(
            station(&reply),
            Some(Station {
                bssid: Some([1, 2, 3, 4, 5, 6]),
                signal: Some(-45),
            })
        );
    }

    #[test]
    fn an_ssid_cache_entry_belongs_to_one_interface_and_access_point() {
        let cached = CachedNetwork {
            ifindex: 3,
            bssid: [1, 2, 3, 4, 5, 6],
            ssid: "Cafe".to_string(),
        };
        assert!(cached.is_for(3, [1, 2, 3, 4, 5, 6]));
        assert!(!cached.is_for(4, [1, 2, 3, 4, 5, 6]));
        assert!(!cached.is_for(3, [6, 5, 4, 3, 2, 1]));
    }

    #[test]
    fn only_an_associated_scan_result_supplies_its_ssid() {
        let reply = scan_reply(NL80211_BSS_STATUS_ASSOCIATED, Some(b"\x00\x04Cafe"), None);
        assert_eq!(associated_ssid(&reply).as_deref(), Some("Cafe"));

        let authenticated = scan_reply(0, Some(b"\x00\x0aNot joined"), None);
        assert_eq!(associated_ssid(&authenticated), None);
    }

    #[test]
    fn beacon_ies_supply_the_ssid_when_probe_ies_do_not() {
        let reply = scan_reply(
            NL80211_BSS_STATUS_IBSS_JOINED,
            Some(&[1, 1, 0x82]),
            Some(b"\x00\x04mesh"),
        );
        assert_eq!(associated_ssid(&reply).as_deref(), Some("mesh"));
    }

    #[test]
    fn a_malformed_information_element_is_not_read_past_its_length() {
        assert_eq!(ssid_from_ies(&[1, 8, 1, 2]), None);
        assert_eq!(ssid_from_ies(&[1]), None);
    }

    fn scan_reply(status: u32, probe_ies: Option<&[u8]>, beacon_ies: Option<&[u8]>) -> Vec<u8> {
        let mut bss = Vec::new();
        bss.extend_from_slice(&attribute(NL80211_BSS_STATUS, &status.to_ne_bytes()));
        if let Some(ies) = probe_ies {
            bss.extend_from_slice(&attribute(NL80211_BSS_INFORMATION_ELEMENTS, ies));
        }
        if let Some(ies) = beacon_ies {
            bss.extend_from_slice(&attribute(NL80211_BSS_BEACON_IES, ies));
        }

        let mut reply = genl_header(NL80211_CMD_GET_SCAN, 0).to_vec();
        reply.extend_from_slice(&attribute(NL80211_ATTR_BSS | 0x8000, &bss));
        reply
    }
}
