//! A small D-Bus client, for the things a bar can only learn by asking the session bus.
//!
//! This is not a general D-Bus library and does not want to be. It speaks exactly as much
//! of the protocol as a status bar needs: connect to the session bus, call a method, watch
//! for signals. That is a few hundred lines, where every D-Bus crate worth the name brings
//! an async runtime with it - and an event loop is something dbar already has.
//!
//! What is deliberately missing: file descriptor passing, the SHA-1 authentication nobody
//! uses on a local socket, and writing anything but strings. Reading has to be general,
//! because a property is a variant and the bus decides what is in it; writing does not,
//! because every call the bar makes takes a string or nothing at all.

use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use anyhow::{Context as _, Result, bail};

/// A value read off the bus.
///
/// The numeric kinds are collapsed where nothing the bar reads tells them apart: what
/// matters at the far end is whether it is a number, a string, or something with parts.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// An array, or a struct: both are a run of values, and no caller here cares which.
    Seq(Vec<Value>),
    /// `a{sv}` and friends, which is how every property bundle arrives.
    Map(Vec<(Value, Value)>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The value stored under a string key, for the dictionaries D-Bus answers with.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// The first string in this value, whether it is one or a list of them.
    ///
    /// An MPRIS artist is a list because a track can have several, and a bar has room for
    /// one; a title is a plain string. Both arrive as a variant, so both are asked the
    /// same question.
    pub fn first_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            Value::Seq(values) => values.iter().find_map(Value::first_str),
            _ => None,
        }
    }
}

/// What kind of message this is. The bar only distinguishes what it acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    MethodCall,
    Return,
    Error,
    Signal,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub kind: Kind,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub sender: Option<String>,
    pub error: Option<String>,
    pub reply_serial: Option<u32>,
    pub body: Vec<Value>,
}

impl Message {
    /// Whether this is the signal it says it is, which is the question every handler asks.
    pub fn is_signal(&self, interface: &str, member: &str) -> bool {
        self.kind == Kind::Signal
            && self.interface.as_deref() == Some(interface)
            && self.member.as_deref() == Some(member)
    }
}

pub struct Connection {
    socket: UnixStream,
    serial: u32,
    /// Left over from a read that took in more than one message, since the bus is free to
    /// send them back to back.
    pending: Vec<u8>,
}

impl Connection {
    /// Connect to the session bus and say hello, which is what earns a name on it.
    pub fn session() -> Result<Connection> {
        let address = std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .context("DBUS_SESSION_BUS_ADDRESS is not set; is there a session bus?")?;
        let path = socket_path(&address)
            .with_context(|| format!("no unix socket in the bus address {address:?}"))?;
        let socket = UnixStream::connect(&path)
            .with_context(|| format!("connecting to the session bus at {path}"))?;

        let mut connection = Connection {
            socket,
            serial: 0,
            pending: Vec::new(),
        };
        connection.authenticate()?;
        connection
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "Hello",
                &[],
            )
            .context("saying hello to the session bus")?;
        Ok(connection)
    }

    /// The socket, so a caller can wait on it alongside anything else it is watching.
    pub fn fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }

    /// Call a method and wait for its answer.
    ///
    /// Signals that arrive first are dropped: this is used while setting up, before
    /// anything is watching for them.
    pub fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        arguments: &[&str],
    ) -> Result<Vec<Value>> {
        let serial = self.send(destination, path, interface, member, arguments)?;
        loop {
            let message = self.receive()?;
            if message.reply_serial != Some(serial) {
                continue;
            }
            if message.kind == Kind::Error {
                let name = message.error.as_deref().unwrap_or("an unnamed error");
                let detail = message.body.first().and_then(Value::as_str).unwrap_or("");
                bail!("{member} failed: {name} {detail}");
            }
            return Ok(message.body);
        }
    }

    /// Send a method call and do not wait for the answer.
    pub fn send(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        arguments: &[&str],
    ) -> Result<u32> {
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;

        let mut body = Writer::new();
        for argument in arguments {
            body.string(argument);
        }
        let signature: String = std::iter::repeat_n('s', arguments.len()).collect();

        let mut fields = vec![
            (1u8, 'o', path.to_string()),
            (2u8, 's', interface.to_string()),
            (3u8, 's', member.to_string()),
            (6u8, 's', destination.to_string()),
        ];
        if !arguments.is_empty() {
            fields.push((8u8, 'g', signature));
        }

        let mut out = Writer::new();
        out.byte(b'l');
        out.byte(1); // method call
        out.byte(0); // no flags: a reply is wanted
        out.byte(1); // protocol version
        out.u32(body.bytes.len() as u32);
        out.u32(serial);
        out.header_fields(&fields);
        out.align(8);
        out.bytes.extend_from_slice(&body.bytes);

        self.socket
            .write_all(&out.bytes)
            .with_context(|| format!("sending {member}"))?;
        Ok(serial)
    }

    /// Ask the bus to deliver signals matching a rule.
    pub fn add_match(&mut self, rule: &str) -> Result<()> {
        self.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            &[rule],
        )
        .with_context(|| format!("watching for {rule}"))?;
        Ok(())
    }

    /// Read one message, waiting for it if none has arrived.
    pub fn receive(&mut self) -> Result<Message> {
        loop {
            if let Some(message) = self.take_message()? {
                return Ok(message);
            }
            let mut chunk = [0u8; 4096];
            let read = self
                .socket
                .read(&mut chunk)
                .context("reading from the bus")?;
            if read == 0 {
                bail!("the session bus closed the connection");
            }
            self.pending.extend_from_slice(&chunk[..read]);
        }
    }

    /// One message out of what has already arrived, if a whole one is there.
    fn take_message(&mut self) -> Result<Option<Message>> {
        const HEADER: usize = 16;
        if self.pending.len() < HEADER {
            return Ok(None);
        }
        // The length of the fields array sits at the end of the fixed header, and the body
        // follows it once padded to eight.
        let fields_length = u32::from_le_bytes([
            self.pending[12],
            self.pending[13],
            self.pending[14],
            self.pending[15],
        ]) as usize;
        let body_length = u32::from_le_bytes([
            self.pending[4],
            self.pending[5],
            self.pending[6],
            self.pending[7],
        ]) as usize;
        let total = (HEADER + fields_length).div_ceil(8) * 8 + body_length;
        if self.pending.len() < total {
            return Ok(None);
        }

        let raw: Vec<u8> = self.pending.drain(..total).collect();
        parse_message(&raw).map(Some)
    }

    /// The EXTERNAL handshake, which proves who we are with the credentials the kernel
    /// already attached to the socket.
    fn authenticate(&mut self) -> Result<()> {
        // A nul byte first, before anything else, or the bus will not talk at all.
        let uid = unsafe { libc::getuid() };
        let hex: String = uid
            .to_string()
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.socket.write_all(&[0])?;
        self.socket
            .write_all(format!("AUTH EXTERNAL {hex}\r\n").as_bytes())?;

        let reply = self.read_line()?;
        if !reply.starts_with("OK") {
            bail!("the session bus refused the connection: {}", reply.trim());
        }
        self.socket.write_all(b"BEGIN\r\n")?;
        Ok(())
    }

    /// A line of the authentication conversation, which is text rather than messages.
    fn read_line(&mut self) -> Result<String> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while line.len() < 512 {
            let read = self
                .socket
                .read(&mut byte)
                .context("reading from the bus")?;
            if read == 0 {
                bail!("the session bus closed the connection during authentication");
            }
            line.push(byte[0]);
            if line.ends_with(b"\r\n") {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&line).into_owned())
    }
}

/// The socket in a bus address, which may list several ways to connect.
fn socket_path(address: &str) -> Option<String> {
    address.split(';').find_map(|one| {
        let rest = one.trim().strip_prefix("unix:")?;
        rest.split(',').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            match key {
                "path" => Some(value.to_string()),
                // An abstract socket is named with a leading nul, which Rust spells as a
                // zero byte in the path.
                "abstract" => Some(format!("\0{value}")),
                _ => None,
            }
        })
    })
}

fn parse_message(raw: &[u8]) -> Result<Message> {
    if raw.len() < 16 {
        bail!("a message too short to hold a header");
    }
    if raw[0] != b'l' {
        // Every implementation in use is little-endian, and a big-endian one would need
        // its own reader rather than a swapped byte here and there.
        bail!("the bus is speaking big-endian, which dbar does not read");
    }
    let kind = match raw[1] {
        1 => Kind::MethodCall,
        2 => Kind::Return,
        3 => Kind::Error,
        4 => Kind::Signal,
        other => bail!("a message of unknown type {other}"),
    };
    let mut reader = Reader::new(raw);
    reader.position = 12;
    let mut message = Message {
        kind,
        path: None,
        interface: None,
        member: None,
        sender: None,
        error: None,
        reply_serial: None,
        body: Vec::new(),
    };

    // The header fields: an array of (field code, variant).
    let fields_length = reader.u32()? as usize;
    let end = reader.position + fields_length;
    let mut signature = String::new();
    while reader.position < end {
        reader.align(8);
        let code = reader.byte()?;
        let value = reader.variant()?;
        match (code, value) {
            (1, Value::Str(v)) => message.path = Some(v),
            (2, Value::Str(v)) => message.interface = Some(v),
            (3, Value::Str(v)) => message.member = Some(v),
            (4, Value::Str(v)) => message.error = Some(v),
            (5, Value::Int(v)) => message.reply_serial = Some(v as u32),
            (7, Value::Str(v)) => message.sender = Some(v),
            (8, Value::Str(v)) => signature = v,
            _ => {}
        }
    }

    reader.align(8);
    let mut rest = signature.as_str();
    while !rest.is_empty() {
        let (one, tail) = split_type(rest)?;
        message.body.push(reader.value(one)?);
        rest = tail;
    }
    Ok(message)
}

/// The first complete type in a signature, and what is left after it.
fn split_type(signature: &str) -> Result<(&str, &str)> {
    let bytes = signature.as_bytes();
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => {
                depth = depth
                    .checked_sub(1)
                    .context("a signature closes a bracket it never opened")?;
                if depth == 0 {
                    return Ok(signature.split_at(index + 1));
                }
            }
            // An array's type is whatever follows it, so it does not end here.
            b'a' => continue,
            _ if depth == 0 => return Ok(signature.split_at(index + 1)),
            _ => {}
        }
    }
    if depth == 0 && !signature.is_empty() {
        return Ok((signature, ""));
    }
    bail!("a signature that never finishes: {signature:?}")
}

/// Reads the marshalled form, one value at a time, following a signature.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, position: 0 }
    }

    fn align(&mut self, to: usize) {
        self.position = self.position.div_ceil(to) * to;
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.position + count;
        if end > self.bytes.len() {
            bail!("a message that ends in the middle of a value");
        }
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        self.align(4);
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn string(&mut self, length_is_a_byte: bool) -> Result<String> {
        let length = match length_is_a_byte {
            true => self.byte()? as usize,
            false => self.u32()? as usize,
        };
        let bytes = self.take(length)?;
        let text = String::from_utf8_lossy(bytes).into_owned();
        // Strings are stored with a terminator that is not counted in the length.
        self.take(1)?;
        Ok(text)
    }

    /// A variant: its own signature, then one value of that type.
    fn variant(&mut self) -> Result<Value> {
        let signature = self.string(true)?;
        let (one, _) = split_type(&signature)?;
        self.value(one)
    }

    /// One value of the type this signature describes.
    fn value(&mut self, signature: &str) -> Result<Value> {
        let mut characters = signature.chars();
        let kind = characters.next().context("an empty type")?;
        let inner = characters.as_str();
        Ok(match kind {
            'y' => Value::Int(self.byte()? as i64),
            'b' => Value::Bool(self.u32()? != 0),
            'n' | 'q' => {
                self.align(2);
                let bytes = self.take(2)?;
                let raw = u16::from_le_bytes([bytes[0], bytes[1]]);
                match kind {
                    'n' => Value::Int(raw as i16 as i64),
                    _ => Value::Int(raw as i64),
                }
            }
            'i' | 'u' | 'h' => {
                let raw = self.u32()?;
                match kind {
                    'i' => Value::Int(raw as i32 as i64),
                    _ => Value::Int(raw as i64),
                }
            }
            'x' | 't' | 'd' => {
                self.align(8);
                let bytes = self.take(8)?;
                let raw = u64::from_le_bytes(bytes.try_into().expect("eight bytes"));
                match kind {
                    'x' => Value::Int(raw as i64),
                    't' => Value::Int(raw as i64),
                    _ => Value::Float(f64::from_bits(raw)),
                }
            }
            's' | 'o' => Value::Str(self.string(false)?),
            'g' => Value::Str(self.string(true)?),
            'v' => self.variant()?,
            'a' => {
                let length = self.u32()? as usize;
                let element = split_type(inner)?.0;
                // The array's length counts its contents, which start after the padding
                // its element type asks for.
                self.align(alignment_of(element));
                let end = self.position + length;
                if end > self.bytes.len() {
                    bail!("an array longer than the message holding it");
                }
                let dictionary = element.starts_with('{');
                let mut values = Vec::new();
                let mut entries = Vec::new();
                while self.position < end {
                    match dictionary {
                        true => {
                            self.align(8);
                            let inside = &element[1..element.len() - 1];
                            let (key_type, value_type) = split_type(inside)?;
                            let key = self.value(key_type)?;
                            let value = self.value(value_type)?;
                            entries.push((key, value));
                        }
                        false => values.push(self.value(element)?),
                    }
                }
                match dictionary {
                    true => Value::Map(entries),
                    false => Value::Seq(values),
                }
            }
            '(' => {
                self.align(8);
                let inside = &signature[1..signature.len() - 1];
                let mut rest = inside;
                let mut values = Vec::new();
                while !rest.is_empty() {
                    let (one, tail) = split_type(rest)?;
                    values.push(self.value(one)?);
                    rest = tail;
                }
                Value::Seq(values)
            }
            other => bail!("a type dbar does not read: {other}"),
        })
    }
}

/// What a value of this type has to start on.
fn alignment_of(signature: &str) -> usize {
    match signature.chars().next() {
        Some('y') | Some('g') | Some('v') => 1,
        Some('n') | Some('q') => 2,
        Some('x') | Some('t') | Some('d') | Some('(') | Some('{') => 8,
        _ => 4,
    }
}

/// Writes the marshalled form. Only what the bar sends: strings, and the header around
/// them.
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn new() -> Writer {
        Writer { bytes: Vec::new() }
    }

    fn align(&mut self, to: usize) {
        self.bytes.resize(self.bytes.len().div_ceil(to) * to, 0);
    }

    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.align(4);
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
    }

    /// A signature, whose length is a single byte because it can never be long.
    fn signature(&mut self, value: &str) {
        self.byte(value.len() as u8);
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
    }

    /// The array of (code, variant) pairs that says what a message is.
    fn header_fields(&mut self, fields: &[(u8, char, String)]) {
        // The array's contents are measured from where they start, and each field inside
        // begins on an eight-byte boundary of its own.
        let mut inner = Writer::new();
        for (code, kind, value) in fields {
            inner.align(8);
            inner.byte(*code);
            inner.signature(&kind.to_string());
            match kind {
                'g' => inner.signature(value),
                _ => inner.string(value),
            }
        }
        self.u32(inner.bytes.len() as u32);
        self.bytes.extend_from_slice(&inner.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bus_address_names_its_socket() {
        assert_eq!(
            socket_path("unix:path=/run/user/1000/bus").as_deref(),
            Some("/run/user/1000/bus")
        );
        assert_eq!(
            socket_path("unix:path=/run/user/1000/bus,guid=deadbeef").as_deref(),
            Some("/run/user/1000/bus")
        );
        // Several addresses, and the first usable one wins.
        assert_eq!(
            socket_path("tcp:host=localhost,port=1;unix:path=/tmp/bus").as_deref(),
            Some("/tmp/bus")
        );
        assert_eq!(socket_path("tcp:host=localhost,port=1"), None);
    }

    #[test]
    fn a_signature_is_split_into_whole_types() {
        assert_eq!(split_type("s").unwrap(), ("s", ""));
        assert_eq!(split_type("sv").unwrap(), ("s", "v"));
        assert_eq!(split_type("a{sv}s").unwrap(), ("a{sv}", "s"));
        assert_eq!(split_type("(sub)a{ss}").unwrap(), ("(sub)", "a{ss}"));
        assert_eq!(split_type("aas").unwrap(), ("aas", ""));
        assert!(split_type("a{sv").is_err());
    }

    /// Marshal what the bus would send for `a{sv}`, so the reader is tested against the
    /// shape a property bundle actually arrives in.
    fn properties(entries: &[(&str, char, &str)]) -> Vec<u8> {
        let mut out = Writer::new();
        // The length comes first, then the entries on the eight-byte boundary a dict
        // entry has to start on. Building it in one buffer is the point: an array written
        // somewhere else and pasted in lands on different padding.
        out.u32(0);
        out.align(8);
        let start = out.bytes.len();
        for (key, kind, value) in entries {
            out.align(8);
            out.string(key);
            out.signature(&kind.to_string());
            match kind {
                's' => out.string(value),
                'b' => out.u32(u32::from(*value == "true")),
                _ => panic!("the fixture only writes strings and booleans"),
            }
        }
        let length = (out.bytes.len() - start) as u32;
        out.bytes[..4].copy_from_slice(&length.to_le_bytes());
        out.bytes
    }

    #[test]
    fn a_property_bundle_reads_back_as_a_map() {
        let bytes = properties(&[
            ("PlaybackStatus", 's', "Playing"),
            ("CanPlay", 'b', "true"),
            ("Identity", 's', "A Player"),
        ]);
        let value = Reader::new(&bytes)
            .value("a{sv}")
            .expect("the fixture is well formed");

        assert_eq!(
            value.get("PlaybackStatus").and_then(Value::as_str),
            Some("Playing")
        );
        assert_eq!(value.get("CanPlay"), Some(&Value::Bool(true)));
        assert_eq!(
            value.get("Identity").and_then(Value::as_str),
            Some("A Player")
        );
        assert_eq!(value.get("Missing"), None);
    }

    #[test]
    fn an_artist_is_read_whether_it_is_one_name_or_a_list() {
        let one = Value::Str("Someone".to_string());
        assert_eq!(one.first_str(), Some("Someone"));

        let several = Value::Seq(vec![
            Value::Str("First".to_string()),
            Value::Str("Second".to_string()),
        ]);
        assert_eq!(several.first_str(), Some("First"));

        assert_eq!(Value::Seq(Vec::new()).first_str(), None);
    }

    #[test]
    fn a_truncated_message_is_an_error_rather_than_a_panic() {
        let bytes = properties(&[("PlaybackStatus", 's', "Playing")]);
        for cut in 1..bytes.len() {
            // Whatever it stops at, it says so rather than reading past the end.
            let _ = Reader::new(&bytes[..cut]).value("a{sv}");
        }
    }
}
