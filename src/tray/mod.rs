//! The system tray: the applications that ask a bar to show an icon for them.
//!
//! The protocol is StatusNotifierItem. An application registers itself with a *watcher*,
//! and every bar that draws a tray is a *host* reading the watcher's list. On a session
//! with no desktop shell there is no watcher at all, so dbar provides one - which is why
//! a tray module has to be configured before any of this is started: owning that name
//! without drawing anything would leave applications registered with a bar that will never
//! show them, and would keep a real tray from taking the name.
//!
//! Everything here runs on a thread of its own, because a bus connection blocks and the
//! bar must not. What comes back is the finished list of items, with their icons already
//! pixels; nothing below this file knows the protocol exists.

pub mod icon;

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::mpsc;

use anyhow::{Context as _, Result};

use crate::dbus::{Arg, Connection, Kind, Message, NameRequest, Value};
use crate::icon::Raster;
use crate::status::{FieldSpec, Kind as FieldKind};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
const ITEM_PATH: &str = "/StatusNotifierItem";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
const BUS: &str = "org.freedesktop.DBus";

/// What a tray module's format can name.
///
/// An item is mostly an icon, but the wording around it is the config's business: a bar
/// that wants the application's name beside the picture should be able to ask for it.
pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "title",
        kind: FieldKind::Text,
    },
    FieldSpec {
        name: "id",
        kind: FieldKind::Text,
    },
    FieldSpec {
        name: "status",
        kind: FieldKind::Text,
    },
];

/// How an item says it is doing, which is the one piece of state a style can key on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// Ordinary. Some hosts hide these; dbar shows them, because a bar the user configured
    /// a tray on is a bar that was asked to show what is there.
    Passive,
    #[default]
    Active,
    /// The application wants to be noticed, which a style can pick up as urgent.
    NeedsAttention,
}

impl Status {
    fn parse(text: &str) -> Status {
        match text {
            "Passive" => Status::Passive,
            "NeedsAttention" => Status::NeedsAttention,
            _ => Status::Active,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Status::Passive => "passive",
            Status::Active => "active",
            Status::NeedsAttention => "attention",
        }
    }
}

/// One application's presence in the tray, as the bar draws it.
#[derive(Clone, Debug)]
pub struct Item {
    /// What the bar calls this item, and what a click names when it comes back.
    pub key: String,
    /// The application's own name for itself, for a format that wants words.
    pub id: String,
    pub title: String,
    pub status: Status,
    /// Shared rather than copied: the icon outlives the frames that draw it, and only
    /// changes when the application says it has.
    pub icon: Option<Arc<Raster>>,
}

/// Everything the tray is showing, in the order the items registered.
#[derive(Clone, Debug, Default)]
pub struct TrayState {
    pub items: Vec<Item>,
}

#[derive(Debug)]
pub enum Event {
    State(Box<TrayState>),
    Stopped(String),
}

/// What a click on an item asks of it.
#[derive(Clone, Debug)]
pub enum Command {
    Activate { key: String, x: i32, y: i32 },
    Secondary { key: String, x: i32, y: i32 },
    Scroll { key: String, delta: i32 },
}

/// The way into the tray thread, which is blocked on the bus and cannot be interrupted any
/// other way.
///
/// A pipe to wake it and a channel for what to do, because a click carries more than a
/// byte and `poll()` takes descriptors rather than channels.
pub struct Commands {
    pipe: OwnedFd,
    queue: mpsc::Sender<Command>,
}

impl Commands {
    pub fn send(&self, command: Command) {
        if self.queue.send(command).is_err() {
            log::debug!("the tray thread is not listening");
            return;
        }
        // SAFETY: a write of one byte from a buffer owned here, to a descriptor this
        // struct owns.
        let written = unsafe { libc::write(self.pipe.as_raw_fd(), [0u8].as_ptr().cast(), 1) };
        if written != 1 {
            log::debug!("the tray thread did not wake");
        }
    }
}

/// Start watching the bus for tray items, at the icon size the bar will draw them.
pub fn spawn(
    sender: calloop::channel::Sender<Event>,
    size: u32,
    theme: String,
) -> Result<Commands> {
    let (read, write) = pipe()?;
    let (queue, orders) = mpsc::channel();
    let report = sender.clone();
    std::thread::Builder::new()
        .name("tray".to_string())
        .spawn(move || {
            if let Err(e) = run(&sender, read, orders, size, &theme) {
                log::warn!("the tray has stopped: {e:#}");
                let _ = report.send(Event::Stopped(format!("{e:#}")));
            }
        })
        .context("spawning the tray thread")?;
    Ok(Commands { pipe: write, queue })
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: the array is owned here and is the length the call requires.
    let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) };
    if made < 0 {
        return Err(std::io::Error::last_os_error()).context("making a pipe for tray commands");
    }
    // SAFETY: both descriptors are fresh, checked, and owned by nothing else.
    unsafe {
        use std::os::fd::FromRawFd as _;
        Ok((OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])))
    }
}

/// One registered application, and everything read about it.
struct Tracked {
    /// The bus name it answers on, which is what a call to it is addressed to.
    service: String,
    /// The object inside that name, which is not always the one the spec suggests.
    path: String,
    item: Item,
    /// What its icon was resolved from, so a `NewIcon` that changes nothing changes
    /// nothing: applications announce an icon far more often than they change one.
    seen: Option<Seen>,
}

/// What an icon was made of, for telling a real change from a repeated announcement.
#[derive(PartialEq, Eq)]
enum Seen {
    Named(String, Option<String>),
    Pixels(u64),
}

/// Everything the thread keeps while it runs.
struct Tray {
    items: Vec<Tracked>,
    /// Whether dbar is the watcher, and so has to answer for one.
    hosting: bool,
    size: u32,
    theme: String,
}

fn run(
    sender: &calloop::channel::Sender<Event>,
    wake: OwnedFd,
    orders: mpsc::Receiver<Command>,
    size: u32,
    theme: &str,
) -> Result<()> {
    let mut bus = Connection::session()?;
    let mut tray = Tray {
        items: Vec::new(),
        hosting: false,
        size,
        theme: theme.to_string(),
    };

    // Whoever gets the name is the watcher. Losing the race is not a failure: it means a
    // real tray is already running, and dbar is then only one of its hosts.
    match bus.request_name(WATCHER_NAME, 0) {
        Ok(NameRequest::Owner) | Ok(NameRequest::AlreadyOurs) => {
            tray.hosting = true;
            log::info!("no tray watcher on this session, so dbar is providing one");
        }
        Ok(other) => log::info!("another tray watcher is running ({other:?}); following it"),
        Err(e) => log::warn!("could not ask for the watcher name: {e:#}"),
    }

    // A host name of dbar's own, which is what applications look for before they bother
    // drawing anything at all.
    let host = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    if let Err(e) = bus.request_name(&host, 0) {
        log::warn!("could not take a tray host name: {e:#}");
    }

    for rule in [
        format!("type='signal',interface='{WATCHER_NAME}'"),
        format!("type='signal',interface='{ITEM_INTERFACE}'"),
        format!("type='signal',interface='{BUS}',member='NameOwnerChanged'"),
    ] {
        bus.add_match(&rule)?;
    }

    if tray.hosting {
        // Announce to anything already waiting that a host has arrived.
        let _ = bus.emit(
            WATCHER_PATH,
            WATCHER_NAME,
            "StatusNotifierHostRegistered",
            &[],
        );
    } else {
        let _ = bus.call(
            WATCHER_NAME,
            WATCHER_PATH,
            WATCHER_NAME,
            "RegisterStatusNotifierHost",
            &[Arg::Str(&host)],
        );
        adopt_existing(&mut bus, &mut tray);
    }

    publish(sender, &tray);

    loop {
        // Anything a call set aside is already in hand and must be dealt with before
        // sleeping on a socket that may have nothing left to say.
        if !bus.has_deferred() {
            let mut fds = [
                libc::pollfd {
                    fd: bus.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: both descriptors are owned by this thread and outlive the call, and
            // the length is the length of the array they are in.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("waiting on the session bus");
            }
            if fds[1].revents != 0 {
                let mut buffer = [0u8; 64];
                // SAFETY: the buffer is owned here and the length is its own.
                let read = unsafe {
                    libc::read(wake.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                };
                if read <= 0 {
                    return Ok(());
                }
                while let Ok(command) = orders.try_recv() {
                    act(&mut bus, &tray, &command);
                }
            }
            if fds[0].revents == 0 {
                continue;
            }
        }

        let message = bus.receive()?;
        if handle(&mut bus, &mut tray, &message) {
            publish(sender, &tray);
        }
    }
}

/// Take on the items a watcher that was already running knows about.
fn adopt_existing(bus: &mut Connection, tray: &mut Tray) {
    let reply = bus.call(
        WATCHER_NAME,
        WATCHER_PATH,
        PROPERTIES,
        "Get",
        &[
            Arg::Str(WATCHER_NAME),
            Arg::Str("RegisteredStatusNotifierItems"),
        ],
    );
    let Ok(values) = reply else {
        return;
    };
    let names: Vec<String> = values
        .first()
        .map(|v| {
            v.items()
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for name in names {
        let (service, path) = split_registration(&name, &name);
        add(bus, tray, &service, &path);
    }
}

/// Deal with one message, and say whether what the bar draws has changed.
fn handle(bus: &mut Connection, tray: &mut Tray, message: &Message) -> bool {
    if message.kind == Kind::MethodCall {
        return serve(bus, tray, message);
    }

    if message.is_signal(WATCHER_NAME, "StatusNotifierItemRegistered") {
        // Only worth acting on when someone else is the watcher: dbar's own registrations
        // are already in the list by the time this goes out.
        if !tray.hosting
            && let Some(name) = message.body.first().and_then(Value::as_str)
        {
            let (service, path) = split_registration(name, name);
            return add(bus, tray, &service, &path);
        }
        return false;
    }

    if message.is_signal(WATCHER_NAME, "StatusNotifierItemUnregistered") {
        if let Some(name) = message.body.first().and_then(Value::as_str) {
            let (service, _) = split_registration(name, name);
            return remove(tray, &service);
        }
        return false;
    }

    // An application that went away takes its item with it. This is the only notice some
    // of them give: a crash sends no unregistration.
    if message.is_signal(BUS, "NameOwnerChanged") {
        let name = message.body.first().and_then(Value::as_str);
        let new_owner = message.body.get(2).and_then(Value::as_str);
        if let Some(name) = name
            && new_owner == Some("")
        {
            return remove(tray, name);
        }
        return false;
    }

    if message.kind == Kind::Signal
        && message.interface.as_deref() == Some(ITEM_INTERFACE)
        && let Some(sender) = message.sender.as_deref()
    {
        return refresh(bus, tray, sender);
    }

    false
}

/// Answer a call made to the watcher dbar is providing.
///
/// An object that stays silent leaves the caller waiting for its own timeout, so
/// everything gets an answer, including the calls that are refused.
fn serve(bus: &mut Connection, tray: &mut Tray, message: &Message) -> bool {
    let member = message.member.as_deref().unwrap_or_default();

    if message.is_call(WATCHER_NAME, "RegisterStatusNotifierItem") {
        let argument = message.body.first().and_then(Value::as_str).unwrap_or("");
        let sender = message.sender.as_deref().unwrap_or("");
        let (service, path) = split_registration(argument, sender);
        let _ = bus.reply(message, &[]);
        let changed = add(bus, tray, &service, &path);
        let _ = bus.emit(
            WATCHER_PATH,
            WATCHER_NAME,
            "StatusNotifierItemRegistered",
            &[Arg::Str(&format!("{service}{path}"))],
        );
        return changed;
    }

    if message.is_call(WATCHER_NAME, "RegisterStatusNotifierHost") {
        let _ = bus.reply(message, &[]);
        let _ = bus.emit(
            WATCHER_PATH,
            WATCHER_NAME,
            "StatusNotifierHostRegistered",
            &[],
        );
        return false;
    }

    if message.is_call(PROPERTIES, "Get") {
        let property = message.body.get(1).and_then(Value::as_str).unwrap_or("");
        answer_property(bus, tray, message, property);
        return false;
    }

    if message.is_call(PROPERTIES, "GetAll") {
        let registered = registered_names(tray);
        let names: Vec<Arg> = registered.iter().map(|n| Arg::Str(n)).collect();
        let entries = [
            ("RegisteredStatusNotifierItems", Arg::Array("s", &names)),
            ("IsStatusNotifierHostRegistered", Arg::Bool(true)),
            ("ProtocolVersion", Arg::I32(0)),
        ];
        let _ = bus.reply(message, &[Arg::Dict(&entries)]);
        return false;
    }

    if message.is_call(INTROSPECTABLE, "Introspect") {
        let _ = bus.reply(message, &[Arg::Str(INTROSPECTION)]);
        return false;
    }

    if message.is_call("org.freedesktop.DBus.Peer", "Ping") {
        let _ = bus.reply(message, &[]);
        return false;
    }

    let _ = bus.reply_error(
        message,
        "org.freedesktop.DBus.Error.UnknownMethod",
        &format!("dbar's tray watcher has no {member}"),
    );
    false
}

/// Answer one property of the watcher.
fn answer_property(bus: &mut Connection, tray: &Tray, message: &Message, property: &str) {
    match property {
        "RegisteredStatusNotifierItems" => {
            let registered = registered_names(tray);
            let names: Vec<Arg> = registered.iter().map(|n| Arg::Str(n)).collect();
            let _ = bus.reply(message, &[Arg::Var(&Arg::Array("s", &names))]);
        }
        "IsStatusNotifierHostRegistered" => {
            let _ = bus.reply(message, &[Arg::Var(&Arg::Bool(true))]);
        }
        "ProtocolVersion" => {
            let _ = bus.reply(message, &[Arg::Var(&Arg::I32(0))]);
        }
        other => {
            let _ = bus.reply_error(
                message,
                "org.freedesktop.DBus.Error.UnknownProperty",
                &format!("dbar's tray watcher has no {other}"),
            );
        }
    }
}

fn registered_names(tray: &Tray) -> Vec<String> {
    tray.items
        .iter()
        .map(|t| format!("{}{}", t.service, t.path))
        .collect()
}

/// Which bus name and which object a registration names.
///
/// The spec says an application passes its bus name, and several pass the object path
/// instead - so the argument is read as whichever it looks like, and the sender fills in
/// the half it left out. Getting this wrong is the difference between an icon and nothing.
fn split_registration(argument: &str, sender: &str) -> (String, String) {
    match argument.find('/') {
        // A bare path: the application is the one that sent the message.
        Some(0) => (sender.to_string(), argument.to_string()),
        // A name with a path stuck on the end, which is how a watcher lists them.
        Some(at) => (argument[..at].to_string(), argument[at..].to_string()),
        None => (argument.to_string(), ITEM_PATH.to_string()),
    }
}

/// Start following an item, reading everything about it once.
fn add(bus: &mut Connection, tray: &mut Tray, service: &str, path: &str) -> bool {
    if tray
        .items
        .iter()
        .any(|t| t.service == service && t.path == path)
    {
        return false;
    }
    let key = format!("{service}{path}");
    tray.items.push(Tracked {
        service: service.to_string(),
        path: path.to_string(),
        item: Item {
            key,
            id: String::new(),
            title: String::new(),
            status: Status::Active,
            icon: None,
        },
        seen: None,
    });
    let at = tray.items.len() - 1;
    read_item(bus, tray, at);
    log::debug!("tray item {service}{path}");
    true
}

fn remove(tray: &mut Tray, service: &str) -> bool {
    let before = tray.items.len();
    tray.items.retain(|t| t.service != service);
    before != tray.items.len()
}

/// Read an item again because it said something about itself changed.
fn refresh(bus: &mut Connection, tray: &mut Tray, service: &str) -> bool {
    let Some(at) = tray.items.iter().position(|t| t.service == service) else {
        return false;
    };
    read_item(bus, tray, at)
}

/// Read everything about one item, and say whether any of it is different.
///
/// The answer is what decides whether the bar redraws. Applications announce a new icon
/// far more often than they have one - a network applet does it every time it re-checks
/// the connection - so a change that changes nothing must cost a property read on this
/// thread and nothing at all on the bar's.
fn read_item(bus: &mut Connection, tray: &mut Tray, at: usize) -> bool {
    let (service, path) = (tray.items[at].service.clone(), tray.items[at].path.clone());
    let reply = bus.call(
        &service,
        &path,
        PROPERTIES,
        "GetAll",
        &[Arg::Str(ITEM_INTERFACE)],
    );
    let properties = match reply {
        Ok(values) => match values.into_iter().next() {
            Some(value) => value,
            None => return false,
        },
        Err(e) => {
            log::debug!("{service} did not answer for its tray item: {e:#}");
            return false;
        }
    };

    let text = |key: &str| {
        properties
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let id = text("Id");
    let title = match text("Title") {
        empty if empty.is_empty() => id.clone(),
        title => title,
    };
    let status = Status::parse(&text("Status"));

    let name = properties.get("IconName").and_then(Value::as_str);
    let theme_path = properties.get("IconThemePath").and_then(Value::as_str);
    let pixmaps = properties.get("IconPixmap");
    let wanted = match (name.filter(|n| !n.is_empty()), pixmaps) {
        (Some(name), _) => Some(Seen::Named(
            name.to_string(),
            theme_path.map(str::to_string),
        )),
        (None, Some(value)) => Some(Seen::Pixels(fingerprint(value))),
        (None, None) => None,
    };

    let tracked = &mut tray.items[at];
    let same_icon = tracked.seen == wanted && (tracked.item.icon.is_some() || wanted.is_none());
    let unchanged = same_icon
        && tracked.item.id == id
        && tracked.item.title == title
        && tracked.item.status == status;
    if unchanged {
        return false;
    }
    log::debug!(
        "tray item {} changed:{}{}{}{}",
        tracked.item.key,
        if same_icon { "" } else { " icon" },
        if tracked.item.id == id { "" } else { " id" },
        if tracked.item.title == title {
            ""
        } else {
            " title"
        },
        if tracked.item.status == status {
            ""
        } else {
            " status"
        },
    );

    tracked.item.id = id;
    tracked.item.title = title;
    tracked.item.status = status;
    if !same_icon {
        tracked.item.icon = match &wanted {
            Some(Seen::Named(name, theme_path)) => {
                icon::from_name(name, theme_path.as_deref(), &tray.theme, tray.size).map(Arc::new)
            }
            Some(Seen::Pixels(_)) => pixmaps
                .and_then(|value| icon::from_pixmaps(value, tray.size))
                .map(Arc::new),
            None => None,
        };
        if tracked.item.icon.is_none() {
            log::debug!("no icon found for tray item {}", tracked.item.key);
        }
        tracked.seen = wanted;
    }
    true
}

/// A cheap summary of a pixmap bundle, for telling one icon from another without keeping
/// the bytes of both.
fn fingerprint(value: &Value) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut eat = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    };
    for entry in value.items() {
        for part in entry.items() {
            match part {
                Value::Int(number) => number.to_le_bytes().iter().for_each(|b| eat(*b)),
                Value::Bytes(bytes) => {
                    // Every byte of a large icon is not worth hashing; a stride through it
                    // separates one icon from another just as well.
                    bytes.len().to_le_bytes().iter().for_each(|b| eat(*b));
                    bytes.iter().step_by(7).for_each(|b| eat(*b));
                }
                _ => {}
            }
        }
    }
    hash
}

/// Do what a click asked of the item it landed on.
fn act(bus: &mut Connection, tray: &Tray, command: &Command) {
    let key = match command {
        Command::Activate { key, .. } | Command::Secondary { key, .. } => key,
        Command::Scroll { key, .. } => key,
    };
    let Some(tracked) = tray.items.iter().find(|t| &t.item.key == key) else {
        return;
    };

    // The answer is a reply nobody reads: whatever the application does about it comes
    // back as a property change like any other, which is what the bar then draws.
    let (member, arguments) = match command {
        Command::Activate { x, y, .. } => ("Activate", vec![Arg::I32(*x), Arg::I32(*y)]),
        Command::Secondary { x, y, .. } => ("SecondaryActivate", vec![Arg::I32(*x), Arg::I32(*y)]),
        Command::Scroll { delta, .. } => ("Scroll", vec![Arg::I32(*delta), Arg::Str("vertical")]),
    };
    if let Err(e) = bus.send(
        &tracked.service,
        &tracked.path,
        ITEM_INTERFACE,
        member,
        &arguments,
    ) {
        log::debug!("{member} did not reach {}: {e:#}", tracked.service);
    }
}

fn publish(sender: &calloop::channel::Sender<Event>, tray: &Tray) {
    let items = tray.items.iter().map(|t| t.item.clone()).collect();
    let _ = sender.send(Event::State(Box::new(TrayState { items })));
}

/// What the watcher says about itself when something asks.
///
/// Applications introspect before they register, and one that gets nothing back may decide
/// there is no tray at all.
const INTROSPECTION: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect"><arg name="xml" type="s" direction="out"/></method>
  </interface>
  <interface name="org.freedesktop.DBus.Properties">
    <method name="Get">
      <arg name="interface" type="s" direction="in"/>
      <arg name="property" type="s" direction="in"/>
      <arg name="value" type="v" direction="out"/>
    </method>
    <method name="GetAll">
      <arg name="interface" type="s" direction="in"/>
      <arg name="properties" type="a{sv}" direction="out"/>
    </method>
  </interface>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem"><arg name="service" type="s" direction="in"/></method>
    <method name="RegisterStatusNotifierHost"><arg name="service" type="s" direction="in"/></method>
    <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
    <property name="ProtocolVersion" type="i" access="read"/>
    <signal name="StatusNotifierItemRegistered"><arg name="service" type="s"/></signal>
    <signal name="StatusNotifierItemUnregistered"><arg name="service" type="s"/></signal>
    <signal name="StatusNotifierHostRegistered"/>
  </interface>
</node>"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec says an application passes its bus name; several pass the object path
    /// instead, and a watcher lists them as the two stuck together. Reading any of the
    /// three wrongly is the difference between an icon and nothing at all.
    #[test]
    fn a_registration_is_read_however_the_application_wrote_it() {
        // What flameshot does: its own bus name, and the path the spec suggests.
        assert_eq!(
            split_registration(":1.75", ":1.75"),
            (":1.75".to_string(), "/StatusNotifierItem".to_string())
        );
        // What nm-applet does: a bare object path, so the sender is the application.
        assert_eq!(
            split_registration("/org/ayatana/NotificationItem/nm_applet", ":1.72"),
            (
                ":1.72".to_string(),
                "/org/ayatana/NotificationItem/nm_applet".to_string()
            )
        );
        // What a watcher's own list looks like: the two run together.
        assert_eq!(
            split_registration(":1.72/org/ayatana/NotificationItem/nm_applet", ":1.9"),
            (
                ":1.72".to_string(),
                "/org/ayatana/NotificationItem/nm_applet".to_string()
            )
        );
    }

    #[test]
    fn a_status_is_read_or_assumed_active() {
        assert_eq!(Status::parse("Passive"), Status::Passive);
        assert_eq!(Status::parse("NeedsAttention"), Status::NeedsAttention);
        assert_eq!(Status::parse("Active"), Status::Active);
        // An application that says something else still gets shown.
        assert_eq!(Status::parse("nonsense"), Status::Active);
    }

    fn pixmap(fill: u8) -> Value {
        Value::Seq(vec![Value::Seq(vec![
            Value::Int(2),
            Value::Int(2),
            Value::Bytes(vec![fill; 16]),
        ])])
    }

    /// The whole reason the bar stays asleep: applications announce a new icon far more
    /// often than they have one, and a repeated announcement must not become a redraw.
    #[test]
    fn the_same_icon_announced_twice_looks_the_same() {
        assert_eq!(fingerprint(&pixmap(7)), fingerprint(&pixmap(7)));
        assert_ne!(fingerprint(&pixmap(7)), fingerprint(&pixmap(8)));
        // A different size is a different icon even at the same fill.
        let taller = Value::Seq(vec![Value::Seq(vec![
            Value::Int(2),
            Value::Int(4),
            Value::Bytes(vec![7; 32]),
        ])]);
        assert_ne!(fingerprint(&pixmap(7)), fingerprint(&taller));
    }

    #[test]
    fn an_item_is_named_the_same_way_however_it_registered() {
        let (service, path) = split_registration("/org/ayatana/Item", ":1.4");
        assert_eq!(format!("{service}{path}"), ":1.4/org/ayatana/Item");
        // And that name splits back into the same two halves, which is what lets a
        // watcher's list be read the same way a registration is.
        assert_eq!(
            split_registration(":1.4/org/ayatana/Item", ":1.9"),
            (service, path)
        );
    }
}
