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
use std::time::{Duration, Instant};

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
/// Inside `BSS`: which access point it is, what state it is in, and what it announced.
const NL80211_BSS_BSSID: u16 = 1;
const NL80211_BSS_INFORMATION_ELEMENTS: u16 = 6;
const NL80211_BSS_STATUS: u16 = 9;
const NL80211_BSS_BEACON_IES: u16 = 11;
/// The address of the whole multi-link device, when the BSS is one link of one.
const NL80211_BSS_MLD_ADDR: u16 = 22;
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

/// Dumps in a row that find no name before the retry slows to [`UNNAMED_INTERVAL`].
///
/// A station appears a moment before the scan cache is stamped with the association, so
/// the name is usually there on the next read; a few immediate retries cover that.
const UNNAMED_TRIES: u32 = 3;

/// How long an association the scan cache could not name waits before it is asked about
/// again.
///
/// The retry never stops, because the reason for the miss is not always the association's
/// own: a cache the kernel has not filled in yet fills in later, and a name that arrives
/// late still belongs on the bar. Waiting this long between attempts is what keeps that
/// from costing a dump on every tick, and it is a wait rather than a count of reads so
/// that the cost does not follow whatever `interval` the module was given.
const UNNAMED_INTERVAL: Duration = Duration::from_secs(30);

/// What the last scan-cache dump found, and which association it was for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedNetwork {
    ifindex: u32,
    bssid: [u8; 6],
    /// Kept even when it is `None`: an association the scan cache cannot name is an answer
    /// too, and one worth remembering rather than dumping the whole cache for again.
    ssid: Option<String>,
    /// Dumps in a row that have found no name for this association, and when the last of
    /// them was. Both are only ever read while `ssid` is `None`.
    misses: u32,
    asked: Instant,
}

impl CachedNetwork {
    fn is_for(&self, ifindex: u32, bssid: [u8; 6]) -> bool {
        self.ifindex == ifindex && self.bssid == bssid
    }
}

/// What a read should do with the association the station dump just described.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Lookup {
    /// The cache already answers for this association.
    Cached(Option<String>),
    /// The scan cache has to be read, after this many dumps that found nothing.
    Dump { misses: u32 },
}

/// Decide between the cache and a scan-cache dump.
///
/// Split out from [`Wireless::state_of`] because it is the whole of the decision a socket
/// is not needed to test: a name is held until the access point changes, and an
/// association with no name is retried at once a few times and slowly after that.
fn lookup(cache: Option<&CachedNetwork>, ifindex: u32, bssid: [u8; 6], now: Instant) -> Lookup {
    let Some(network) = cache.filter(|network| network.is_for(ifindex, bssid)) else {
        return Lookup::Dump { misses: 0 };
    };
    if network.ssid.is_some() {
        return Lookup::Cached(network.ssid.clone());
    }
    if network.misses < UNNAMED_TRIES
        || now.saturating_duration_since(network.asked) >= UNNAMED_INTERVAL
    {
        return Lookup::Dump {
            misses: network.misses,
        };
    }
    Lookup::Cached(None)
}

/// The access point a card is talking to, as a station dump describes it.
///
/// The peer address is not optional. It is what tells one association from the next, and
/// so what the name cache is keyed by; the kernel puts it on every station reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Station {
    bssid: [u8; 6],
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

        let now = Instant::now();
        let misses = match lookup(self.network.as_ref(), ifindex, station.bssid, now) {
            Lookup::Cached(ssid) => return Ok((ssid, station.signal)),
            Lookup::Dump { misses } => misses,
        };

        let ssid = self.network_of(ifindex, station.bssid)?;
        self.network = Some(CachedNetwork {
            ifindex,
            bssid: station.bssid,
            ssid: ssid.clone(),
            misses: if ssid.is_some() { 0 } else { misses + 1 },
            asked: now,
        });
        Ok((ssid, station.signal))
    }

    /// The SSID of the BSS this interface is associated with, from cached scan results.
    fn network_of(&mut self, ifindex: u32, bssid: [u8; 6]) -> Result<Option<String>> {
        let mut request = Vec::new();
        request.extend_from_slice(&genl_header(NL80211_CMD_GET_SCAN, 0));
        request.extend_from_slice(&attribute(NL80211_ATTR_IFINDEX, &ifindex.to_ne_bytes()));
        self.send(self.family, NLM_F_REQUEST | NLM_F_DUMP, &request)?;

        let mut found = None;
        self.receive(true, |payload| {
            if let Some(ssid) = associated_ssid(payload, bssid) {
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
///
/// A reply with no peer address is no use, whatever else it carries: there would be nothing
/// to key the name cache by, and nothing to match a scan result against.
fn station(payload: &[u8]) -> Option<Station> {
    let mut bssid = None;
    let mut signal = None;
    for (kind, value) in attributes(genl_body(payload)) {
        match kind {
            NL80211_ATTR_MAC if value.len() >= 6 => bssid = value[..6].try_into().ok(),
            NL80211_ATTR_STA_INFO => {
                // The station's details are a run of attributes of their own.
                for (inner, bytes) in attributes(value) {
                    if inner == NL80211_STA_INFO_SIGNAL && !bytes.is_empty() {
                        signal = Some(bytes[0] as i8);
                    }
                }
            }
            _ => {}
        }
    }
    Some(Station {
        bssid: bssid?,
        signal,
    })
}

/// Read the SSID from a scan reply only when it describes the current association.
fn associated_ssid(payload: &[u8], bssid: [u8; 6]) -> Option<String> {
    for (kind, value) in attributes(genl_body(payload)) {
        if kind != NL80211_ATTR_BSS {
            continue;
        }

        let mut peer = None;
        let mut mld = None;
        let mut status = None;
        let mut information = None;
        let mut beacon = None;
        for (inner, bytes) in attributes(value) {
            match inner {
                NL80211_BSS_BSSID if bytes.len() >= 6 => peer = bytes[..6].try_into().ok(),
                NL80211_BSS_MLD_ADDR if bytes.len() >= 6 => mld = bytes[..6].try_into().ok(),
                NL80211_BSS_STATUS if bytes.len() >= 4 => {
                    status = Some(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
                }
                NL80211_BSS_INFORMATION_ELEMENTS => information = Some(bytes),
                NL80211_BSS_BEACON_IES => beacon = Some(bytes),
                _ => {}
            }
        }
        // The dump is filtered to this interface, but the cache can hold more than one
        // entry that looks joined, so the entry has to be the access point the station
        // dump named. Either address answers to that name: a multi-link association is a
        // station at the device's own address, while each BSS behind it is one link with a
        // BSSID of its own. An IBSS has no such peer at all - the cell's address belongs
        // to no one station - so a joined cell is taken on its status alone.
        let ours = match status {
            Some(NL80211_BSS_STATUS_ASSOCIATED) => peer == Some(bssid) || mld == Some(bssid),
            Some(NL80211_BSS_STATUS_IBSS_JOINED) => true,
            _ => false,
        };
        if !ours {
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
            // A hidden network's beacon carries the element with nothing in it, which is
            // the absence of a name rather than a network named "". Saying so here is what
            // lets the other set of information elements be tried.
            return (!value.is_empty()).then(|| String::from_utf8_lossy(value).into_owned());
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

    const AP: [u8; 6] = [1, 2, 3, 4, 5, 6];
    const OTHER_AP: [u8; 6] = [6, 5, 4, 3, 2, 1];

    #[test]
    fn a_station_reply_carries_the_cache_key_and_signal() {
        let mut details = Vec::new();
        details.extend_from_slice(&attribute(NL80211_STA_INFO_SIGNAL, &[(-45i8) as u8]));

        let mut reply = genl_header(NL80211_CMD_GET_STATION, 0).to_vec();
        reply.extend_from_slice(&attribute(NL80211_ATTR_MAC, &AP));
        reply.extend_from_slice(&attribute(NL80211_ATTR_STA_INFO | 0x8000, &details));

        assert_eq!(
            station(&reply),
            Some(Station {
                bssid: AP,
                signal: Some(-45),
            })
        );
    }

    #[test]
    fn a_station_with_no_signal_is_still_an_association() {
        let mut reply = genl_header(NL80211_CMD_GET_STATION, 0).to_vec();
        reply.extend_from_slice(&attribute(NL80211_ATTR_MAC, &AP));
        reply.extend_from_slice(&attribute(NL80211_ATTR_STA_INFO | 0x8000, &[]));

        assert_eq!(
            station(&reply),
            Some(Station {
                bssid: AP,
                signal: None,
            })
        );
    }

    #[test]
    fn a_station_reply_without_a_peer_address_is_no_station_at_all() {
        let mut details = Vec::new();
        details.extend_from_slice(&attribute(NL80211_STA_INFO_SIGNAL, &[(-45i8) as u8]));

        let mut reply = genl_header(NL80211_CMD_GET_STATION, 0).to_vec();
        reply.extend_from_slice(&attribute(NL80211_ATTR_STA_INFO | 0x8000, &details));

        assert_eq!(station(&reply), None);
    }

    #[test]
    fn an_ssid_cache_entry_belongs_to_one_interface_and_access_point() {
        let cached = named("Cafe");
        assert!(cached.is_for(3, AP));
        assert!(!cached.is_for(4, AP));
        assert!(!cached.is_for(3, OTHER_AP));
    }

    #[test]
    fn a_name_is_held_until_the_access_point_changes() {
        let cached = named("Cafe");
        let much_later = cached.asked + UNNAMED_INTERVAL * 100;
        assert_eq!(
            lookup(Some(&cached), 3, AP, much_later),
            Lookup::Cached(Some("Cafe".to_string()))
        );
        assert_eq!(
            lookup(Some(&cached), 3, OTHER_AP, much_later),
            Lookup::Dump { misses: 0 }
        );
        assert_eq!(lookup(None, 3, AP, much_later), Lookup::Dump { misses: 0 });
    }

    /// An association appears a moment before the scan cache can name it, so the first
    /// misses are retried at once - but not every tick after that, or the cache saves
    /// nothing at all.
    #[test]
    fn an_association_with_no_name_is_retried_at_once_only_so_often() {
        let start = Instant::now();
        let mut cache = None;
        let mut dumps = 0;
        // A read every two seconds, which is the interval the module ships with, for as
        // long as one wait lasts: what happens after it is the next test's business.
        for read in 0..UNNAMED_INTERVAL.as_secs() / 2 {
            let now = start + Duration::from_secs(read * 2);
            if let Lookup::Dump { misses } = lookup(cache.as_ref(), 3, AP, now) {
                dumps += 1;
                cache = Some(unnamed(misses + 1, now));
            }
        }
        assert_eq!(dumps, UNNAMED_TRIES);
    }

    /// The scan cache is not always slow for a reason of the association's own, so a name
    /// that turns up late has to reach the bar rather than being shut out for good.
    #[test]
    fn an_association_with_no_name_is_asked_about_again_later() {
        let asked = Instant::now();
        let cache = unnamed(UNNAMED_TRIES, asked);
        assert_eq!(
            lookup(Some(&cache), 3, AP, asked + UNNAMED_INTERVAL / 2),
            Lookup::Cached(None)
        );
        assert_eq!(
            lookup(Some(&cache), 3, AP, asked + UNNAMED_INTERVAL),
            Lookup::Dump {
                misses: UNNAMED_TRIES
            }
        );

        let settled = unnamed(UNNAMED_TRIES * 1000, asked);
        assert_eq!(
            lookup(Some(&settled), 3, AP, asked + UNNAMED_INTERVAL),
            Lookup::Dump {
                misses: UNNAMED_TRIES * 1000
            }
        );
    }

    fn named(ssid: &str) -> CachedNetwork {
        CachedNetwork {
            ifindex: 3,
            bssid: AP,
            ssid: Some(ssid.to_string()),
            misses: 0,
            asked: Instant::now(),
        }
    }

    fn unnamed(misses: u32, asked: Instant) -> CachedNetwork {
        CachedNetwork {
            ifindex: 3,
            bssid: AP,
            ssid: None,
            misses,
            asked,
        }
    }

    #[test]
    fn only_an_associated_scan_result_supplies_its_ssid() {
        let reply = scan_reply(
            AP,
            NL80211_BSS_STATUS_ASSOCIATED,
            Some(b"\x00\x04Cafe"),
            None,
        );
        assert_eq!(associated_ssid(&reply, AP).as_deref(), Some("Cafe"));

        let authenticated = scan_reply(AP, 0, Some(b"\x00\x0aNot joined"), None);
        assert_eq!(associated_ssid(&authenticated, AP), None);
    }

    #[test]
    fn an_associated_entry_for_another_access_point_is_not_ours() {
        let reply = scan_reply(
            OTHER_AP,
            NL80211_BSS_STATUS_ASSOCIATED,
            Some(b"\x00\x08Next door"),
            None,
        );
        assert_eq!(associated_ssid(&reply, AP), None);
    }

    /// An IBSS cell's address belongs to no one station, so the peer cannot be matched
    /// against it and the status has to be enough.
    #[test]
    fn beacon_ies_supply_the_ssid_when_probe_ies_do_not() {
        let reply = scan_reply(
            OTHER_AP,
            NL80211_BSS_STATUS_IBSS_JOINED,
            Some(&[1, 1, 0x82]),
            Some(b"\x00\x04mesh"),
        );
        assert_eq!(associated_ssid(&reply, AP).as_deref(), Some("mesh"));
    }

    #[test]
    fn a_hidden_networks_empty_name_falls_through_to_the_other_elements() {
        assert_eq!(ssid_from_ies(&[0, 0]), None);

        let reply = scan_reply(
            AP,
            NL80211_BSS_STATUS_ASSOCIATED,
            Some(b"\x00\x00"),
            Some(b"\x00\x04Cafe"),
        );
        assert_eq!(associated_ssid(&reply, AP).as_deref(), Some("Cafe"));
    }

    /// A multi-link association is a station at the device's own address, and the BSS
    /// behind it is one link with a BSSID that is not that address.
    #[test]
    fn a_multi_link_association_is_found_by_the_devices_own_address() {
        const MLD: [u8; 6] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];

        let mut bss = Vec::new();
        bss.extend_from_slice(&attribute(NL80211_BSS_BSSID, &AP));
        bss.extend_from_slice(&attribute(NL80211_BSS_MLD_ADDR, &MLD));
        bss.extend_from_slice(&attribute(
            NL80211_BSS_STATUS,
            &NL80211_BSS_STATUS_ASSOCIATED.to_ne_bytes(),
        ));
        bss.extend_from_slice(&attribute(
            NL80211_BSS_INFORMATION_ELEMENTS,
            b"\x00\x04Cafe",
        ));

        let mut reply = genl_header(NL80211_CMD_GET_SCAN, 0).to_vec();
        reply.extend_from_slice(&attribute(NL80211_ATTR_BSS | 0x8000, &bss));

        assert_eq!(associated_ssid(&reply, MLD).as_deref(), Some("Cafe"));
        // The link's own address still answers, which is what a station dump gives when
        // the association is not a multi-link one.
        assert_eq!(associated_ssid(&reply, AP).as_deref(), Some("Cafe"));
        assert_eq!(associated_ssid(&reply, OTHER_AP), None);
    }

    #[test]
    fn a_malformed_information_element_is_not_read_past_its_length() {
        assert_eq!(ssid_from_ies(&[1, 8, 1, 2]), None);
        assert_eq!(ssid_from_ies(&[1]), None);
    }

    fn scan_reply(
        bssid: [u8; 6],
        status: u32,
        probe_ies: Option<&[u8]>,
        beacon_ies: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut bss = Vec::new();
        bss.extend_from_slice(&attribute(NL80211_BSS_BSSID, &bssid));
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
