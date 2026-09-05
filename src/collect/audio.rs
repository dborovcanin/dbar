//! Volume and mute, from PipeWire.
//!
//! Nothing here is sampled. PipeWire tells a client when a node's properties change, so the
//! volume is read exactly when someone moves it and never otherwise - the same bargain the
//! backlight has with sysfs, over a socket instead of a file.
//!
//! A connection to PipeWire is not a thing to hold on the main thread: it owns its own loop
//! and blocks in it. So this runs on a thread and pushes finished readings down a calloop
//! channel, and the event loop treats it as one more source arriving from elsewhere.
//!
//! What is watched is the default sink rather than a named one, because that is what the
//! volume keys move. Which node that is comes from the "default" metadata, so switching
//! output to a headset moves the module without anything being restarted.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Property, Value, ValueArray};
use pw::types::ObjectType;

use super::Reading;
use crate::status::{FieldSpec, Fields, Kind, State, Unit, Value as Field};

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "volume",
        kind: Kind::Num(Unit::Percent),
    },
    // `yes` or `no`, so a state rule reads what was measured rather than searching the
    // text a format produced.
    FieldSpec {
        name: "muted",
        kind: Kind::Text,
    },
    // What is actually playing it: "Speaker", "HDMI", the name of a headset.
    FieldSpec {
        name: "device",
        kind: Kind::Text,
    },
    // Which socket the sound is leaving by, as one of a few known words rather than the
    // card's own wording, so a state rule can match it: `headphones`, `speaker`, `hdmi`,
    // `bluetooth`, `line-out` or `other`. Plugging headphones in changes this.
    FieldSpec {
        name: "port",
        kind: Kind::Text,
    },
];

/// What the bar can ask PipeWire to do, when a person scrolls or clicks on the volume.
pub enum Command {
    /// Move the volume by this many of the percentage points a person sees.
    Volume(f64),
    /// Mute, or unmute, whichever it is not.
    ToggleMute,
}

/// The way back into the PipeWire thread, since the connection cannot be touched from
/// anywhere else.
pub type Commands = pw::channel::Sender<Command>;

/// Start listening to PipeWire, and report readings as they change.
pub fn spawn(sender: calloop::channel::Sender<Reading>) -> Result<Commands> {
    let (commands, receiver) = pw::channel::channel::<Command>();
    std::thread::Builder::new()
        .name("audio".to_string())
        .spawn(move || match run(sender, receiver) {
            Ok(()) => log::info!("PipeWire has gone; the volume module keeps its last reading"),
            Err(e) => log::warn!("volume is unavailable: {e:#}"),
        })
        .context("spawning the audio thread")?;
    Ok(commands)
}

/// The proxies and their listeners, kept alive for as long as the objects they stand for.
///
/// A proxy stops delivering events the moment it is dropped, so binding one and letting it
/// fall out of scope is the same as not binding it at all.
type Bound = Rc<RefCell<HashMap<u32, Vec<Box<dyn pw::proxy::Listener>>>>>;

/// A proxy with nowhere else to live, parked among the listeners so that it lasts as long
/// as they do. A sink's proxy is kept on the sink itself, because changing the volume is a
/// message to it; nothing ever writes to the metadata.
struct Kept<T>(T);

impl<T> pw::proxy::Listener for Kept<T> {}

/// A volume, however it is expressed.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Levels {
    volume: f64,
    muted: bool,
    /// How many channels it is written in. Setting a volume means writing one figure per
    /// channel, and a stereo pod sent to a mono sink is not a volume at all.
    channels: usize,
}

/// One output PipeWire is listing, and what it last said about itself.
struct Sink {
    name: String,
    description: Option<String>,
    /// The node's own volume, which is where a virtual sink keeps it.
    levels: Option<Levels>,
    /// The card this output belongs to, and which of its routes carries this output.
    card: Option<(u32, i32)>,
    /// The proxy, kept because changing the node's volume is a message to it.
    node: pw::node::Node,
}

/// A sound card, and the volume its outputs are actually controlled by.
///
/// A hardware output's volume is a property of the route the card is using, not of the
/// node in front of it: a Bluetooth headset carries its own, and a card with a mixer has
/// one per jack. The node has a volume too, and for these it is a second, quieter gain
/// nobody asked for - so the route is what is read and what is set, which is also what
/// wpctl and pavucontrol do.
struct Card {
    device: pw::device::Device,
    /// By the output index the sink names, since a card has one route per jack.
    routes: HashMap<i32, Route>,
}

struct Route {
    index: i32,
    levels: Levels,
    /// The card's own name for this route, such as `analog-output-headphones`.
    name: Option<String>,
}

/// Which socket a route's own name describes.
///
/// Cards name their routes themselves - `analog-output-headphones`, `hdmi-output-0`,
/// `analog-output-speaker` - and those names vary between drivers. The bar only wants to
/// know which kind of thing is plugged in, so the name is reduced to one of a few words a
/// state rule can match. Anything unrecognised keeps `other` rather than guessing.
fn port_of(name: &str) -> &'static str {
    let name = name.to_ascii_lowercase();
    let has = |needle: &str| name.contains(needle);
    match () {
        _ if has("headphone") || has("headset") => "headphones",
        _ if has("speaker") => "speaker",
        _ if has("hdmi") || has("displayport") => "hdmi",
        _ if has("bluez") || has("bluetooth") || has("a2dp") || has("sco") => "bluetooth",
        _ if has("lineout") || has("line-out") => "line-out",
        _ => "other",
    }
}

/// Everything the thread knows, shared between the callbacks PipeWire calls back into.
struct Sinks {
    /// The node the volume keys move, named by the "default" metadata.
    default: Option<String>,
    by_id: HashMap<u32, Sink>,
    cards: HashMap<u32, Card>,
    sender: calloop::channel::Sender<Reading>,
    /// The last reading sent, so an event that changes nothing does not redraw the bar.
    last: Option<(f64, bool, Option<String>, Option<&'static str>)>,
}

impl Sinks {
    /// The output the volume keys move, which is the one a click here should move too.
    fn current(&self) -> Option<&Sink> {
        let name = self.default.as_ref()?;
        self.by_id.values().find(|sink| &sink.name == name)
    }

    /// The route carrying this output, when its card has one.
    fn route_of(&self, sink: &Sink) -> Option<(&Card, &Route)> {
        let (card_id, output) = sink.card?;
        let card = self.cards.get(&card_id)?;
        Some((card, card.routes.get(&output)?))
    }

    /// Which socket the default output is leaving by, when its card says.
    ///
    /// A node on a card with no routes - a virtual sink, or a Bluetooth device - has no
    /// route to ask, so its own name stands in.
    fn port(&self) -> Option<&'static str> {
        let sink = self.current()?;
        match self.route_of(sink) {
            Some((_, route)) => route.name.as_deref().map(port_of),
            None => Some(port_of(&sink.name)),
        }
    }

    /// What the default output is playing at, from wherever its volume really lives.
    fn levels(&self) -> Option<Levels> {
        let sink = self.current()?;
        match self.route_of(sink) {
            Some((_, route)) => Some(route.levels),
            None => sink.levels,
        }
    }

    /// Send what the default sink is now saying, if it is saying anything new.
    fn publish(&mut self) {
        let Some(levels) = self.levels() else {
            return;
        };
        let description = self.current().and_then(|sink| sink.description.clone());
        let port = self.port();
        // Part of the reading, so plugging headphones in redraws the bar even though the
        // volume and the mute state have not moved.
        let current = (levels.volume, levels.muted, description.clone(), port);
        let (volume, muted) = (levels.volume, levels.muted);
        if self.last.as_ref() == Some(&current) {
            return;
        }
        self.last = Some(current);
        log::debug!(
            "volume {volume:.1}%, muted {muted}, port {}, from {}",
            port.unwrap_or("unknown"),
            match self
                .current()
                .map(|sink| (sink.card, self.route_of(sink).map(|(_, r)| r.index)))
            {
                Some((Some((card, output)), Some(index))) =>
                    format!("card {card} route {index} output {output}"),
                Some((card, _)) => format!("the node itself (card {card:?})"),
                None => "nowhere".to_string(),
            }
        );

        let mut fields = Fields::default();
        fields.set(
            "volume",
            Field::Num {
                v: volume,
                unit: Unit::Percent,
            },
        );
        fields.set(
            "muted",
            Field::Text(match muted {
                true => "yes".to_string(),
                false => "no".to_string(),
            }),
        );
        fields.set(
            "device",
            match description {
                Some(description) => Field::Text(description),
                None => Field::Absent,
            },
        );
        fields.set(
            "port",
            match port {
                Some(port) => Field::Text(port.to_string()),
                None => Field::Absent,
            },
        );
        fields.set_primary("volume");

        // The channel closes when the bar is shutting down, and a volume nobody is going
        // to draw is not worth reporting.
        let _ = self.sender.send(Reading {
            fields,
            state: State::Idle,
        });
    }
}

fn run(
    sender: calloop::channel::Sender<Reading>,
    commands: pw::channel::Receiver<Command>,
) -> Result<()> {
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None).context("creating the PipeWire loop")?;
    let context =
        pw::context::ContextRc::new(&main_loop, None).context("creating the PipeWire context")?;
    let core = context
        .connect_rc(None)
        .context("connecting to PipeWire; is the server running?")?;
    let registry = core
        .get_registry_rc()
        .context("asking PipeWire what it has")?;

    let sinks = Rc::new(RefCell::new(Sinks {
        default: None,
        by_id: HashMap::new(),
        cards: HashMap::new(),
        sender,
        last: None,
    }));

    let proxies: Bound = Rc::new(RefCell::new(HashMap::new()));

    let registry_weak = registry.downgrade();
    let on_global = {
        let sinks = sinks.clone();
        let proxies = proxies.clone();
        move |global: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>| {
            let Some(registry) = registry_weak.upgrade() else {
                return;
            };
            match global.type_ {
                ObjectType::Node => bind_sink(&registry, global, &sinks, &proxies),
                ObjectType::Device => bind_card(&registry, global, &sinks, &proxies),
                ObjectType::Metadata => bind_metadata(&registry, global, &sinks, &proxies),
                _ => {}
            }
        }
    };

    let on_remove = {
        let sinks = sinks.clone();
        let proxies = proxies.clone();
        move |id: u32| {
            proxies.borrow_mut().remove(&id);
            let mut sinks = sinks.borrow_mut();
            let known = sinks.by_id.remove(&id).is_some() | sinks.cards.remove(&id).is_some();
            if known {
                sinks.publish();
            }
        }
    };

    let _listener = registry
        .add_listener_local()
        .global(on_global)
        .global_remove(on_remove)
        .register();

    // What the bar asks for arrives here rather than on the main thread, because the
    // connection may only be touched from the loop that owns it.
    let _commands = {
        let sinks = sinks.clone();
        commands.attach(main_loop.loop_(), move |command| {
            apply(&sinks, command);
        })
    };

    main_loop.run();
    Ok(())
}

/// Follow an output's volume, if this node is one.
fn bind_sink(
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>,
    sinks: &Rc<RefCell<Sinks>>,
    proxies: &Bound,
) {
    let props = global.props;
    if props.and_then(|p| p.get("media.class")) != Some("Audio/Sink") {
        return;
    }
    let Some(name) = props.and_then(|p| p.get("node.name")) else {
        return;
    };
    let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
        return;
    };

    let id = global.id;
    let listener = {
        let sinks = sinks.clone();
        let on_info = {
            let sinks = sinks.clone();
            move |info: &pw::node::NodeInfoRef| {
                // Which card this output belongs to, and which of the card's outputs it
                // is. The registry's summary does not carry either, so it is read here,
                // where a node describes itself in full.
                let card = info.props().and_then(|props| {
                    let device = props.get("device.id")?.parse().ok()?;
                    let output = props.get("card.profile.device")?.parse().ok()?;
                    Some((device, output))
                });
                // An info event carries only what changed, so one that says nothing about
                // the card is not the card going away.
                let mut sinks = sinks.borrow_mut();
                if let (Some(sink), Some(card)) = (sinks.by_id.get_mut(&id), card) {
                    sink.card = Some(card);
                }
                sinks.publish();
            }
        };
        node.add_listener_local()
            .info(on_info)
            .param(move |_seq, _type, _index, _next, param| {
                let Some(param) = param else { return };
                let Some(levels) = levels_of(param) else {
                    return;
                };
                let mut sinks = sinks.borrow_mut();
                if let Some(sink) = sinks.by_id.get_mut(&id) {
                    sink.levels = Some(levels);
                }
                sinks.publish();
            })
            .register()
    };
    // Asking to be told is not the same as being told once: the first values arrive
    // because the subscription itself replays them.
    node.subscribe_params(&[ParamType::Props]);

    sinks.borrow_mut().by_id.insert(
        id,
        Sink {
            name: name.to_string(),
            description: props
                .and_then(|p| p.get("node.description"))
                .map(str::to_string),
            levels: None,
            card: None,
            node,
        },
    );
    proxies.borrow_mut().insert(id, vec![Box::new(listener)]);
}

/// Follow the volumes a sound card keeps for its outputs.
fn bind_card(
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>,
    sinks: &Rc<RefCell<Sinks>>,
    proxies: &Bound,
) {
    if global.props.and_then(|p| p.get("media.class")) != Some("Audio/Device") {
        return;
    }
    let Ok(device) = registry.bind::<pw::device::Device, _>(global) else {
        return;
    };

    let id = global.id;
    let listener = {
        let sinks = sinks.clone();
        let on_info = {
            let sinks = sinks.clone();
            move |info: &pw::device::DeviceInfoRef| {
                // A card does not send a changed route, it says that something among its
                // params moved and waits to be asked. Subscribing only covers the first
                // reply, so every announcement is answered with a fresh enumeration.
                if info
                    .change_mask()
                    .contains(pw::device::DeviceChangeMask::PARAMS)
                    && let Some(card) = sinks.borrow().cards.get(&id)
                {
                    card.device
                        .enum_params(0, Some(ParamType::Route), 0, u32::MAX);
                }
            }
        };
        device
            .add_listener_local()
            .info(on_info)
            .param(move |_seq, _type, _index, _next, param| {
                let Some(param) = param else { return };
                let Some(route) = route_of(param) else { return };
                let mut sinks = sinks.borrow_mut();
                if let Some(card) = sinks.cards.get_mut(&id) {
                    card.routes.insert(route.0, route.1);
                }
                sinks.publish();
            })
            .register()
    };
    device.subscribe_params(&[ParamType::Route]);

    sinks.borrow_mut().cards.insert(
        id,
        Card {
            device,
            routes: HashMap::new(),
        },
    );
    proxies.borrow_mut().insert(id, vec![Box::new(listener)]);
}

/// Follow which output is the default one.
fn bind_metadata(
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>,
    sinks: &Rc<RefCell<Sinks>>,
    proxies: &Bound,
) {
    // PipeWire has several metadata objects; the one that names the defaults is "default".
    if global.props.and_then(|p| p.get("metadata.name")) != Some("default") {
        return;
    }
    let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global) else {
        return;
    };

    let listener = {
        let sinks = sinks.clone();
        metadata
            .add_listener_local()
            .property(move |_subject, key, _type, value| {
                if key != Some("default.audio.sink") {
                    return 0;
                }
                // The value is JSON: {"name": "alsa_output.…"}.
                let name = value
                    .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
                    .and_then(|v| v.get("name")?.as_str().map(str::to_string));
                let mut sinks = sinks.borrow_mut();
                sinks.default = name;
                sinks.publish();
                0
            })
            .register()
    };

    // The proxy has to outlive this function or it stops delivering, and the listener has
    // to outlive the proxy, so both are kept.
    proxies.borrow_mut().insert(
        global.id,
        vec![Box::new(Kept(metadata)), Box::new(listener)],
    );
}

/// The volume and mute a `Props` parameter carries, if it carries them.
fn levels_of(param: &Pod) -> Option<Levels> {
    let (_, value) = PodDeserializer::deserialize_any_from(param.as_bytes()).ok()?;
    let Value::Object(object) = value else {
        return None;
    };
    levels_in(&object)
}

/// The output route a `Route` parameter describes, and which output it belongs to.
///
/// A card answers with every route it has, input ones included, and only the outputs are
/// anything to do with a volume on the bar.
fn route_of(param: &Pod) -> Option<(i32, Route)> {
    let (_, value) = PodDeserializer::deserialize_any_from(param.as_bytes()).ok()?;
    let Value::Object(object) = value else {
        return None;
    };

    let mut index = None;
    let mut output = None;
    let mut levels = None;
    let mut outward = false;
    let mut name = None;
    for property in &object.properties {
        match (property.key, &property.value) {
            (pw::spa::sys::SPA_PARAM_ROUTE_index, Value::Int(v)) => index = Some(*v),
            (pw::spa::sys::SPA_PARAM_ROUTE_device, Value::Int(v)) => output = Some(*v),
            (pw::spa::sys::SPA_PARAM_ROUTE_direction, Value::Id(id)) => {
                outward = id.0 == pw::spa::sys::SPA_DIRECTION_OUTPUT;
            }
            (pw::spa::sys::SPA_PARAM_ROUTE_props, Value::Object(props)) => {
                levels = levels_in(props);
            }
            (pw::spa::sys::SPA_PARAM_ROUTE_name, Value::String(v)) => {
                name = Some(v.clone());
            }
            _ => {}
        }
    }
    if !outward {
        return None;
    }
    Some((
        output?,
        Route {
            index: index?,
            levels: levels?,
            name,
        },
    ))
}

/// The volume and mute inside a `Props` object, wherever that object came from.
fn levels_in(object: &Object) -> Option<Levels> {
    let mut volume = None;
    let mut muted = None;
    let mut channels = 0;
    for property in &object.properties {
        match (property.key, &property.value) {
            (pw::spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(v))) => {
                // Every channel has its own volume and a person has one. The loudest is
                // what they would call the volume; the mean would report a muted right
                // channel as half.
                channels = v.len();
                volume = v
                    .iter()
                    .copied()
                    .fold(None::<f32>, |max, v| Some(max.map_or(v, |m| m.max(v))))
                    .map(percent);
            }
            (pw::spa::sys::SPA_PROP_mute, Value::Bool(m)) => muted = Some(*m),
            _ => {}
        }
    }
    Some(Levels {
        volume: volume?,
        muted: muted?,
        channels,
    })
}

/// Ask the default sink to change, and say nothing about it.
///
/// Nothing is recorded here: the volume on the bar is what PipeWire reports back a moment
/// later, so what is drawn is what the server accepted rather than what was asked for.
fn apply(sinks: &Rc<RefCell<Sinks>>, command: Command) {
    let sinks = sinks.borrow();
    let Some(sink) = sinks.current() else {
        return;
    };
    let Some(levels) = sinks.levels() else { return };
    let property = match command {
        Command::Volume(step) => {
            let shown = levels.volume;
            // The step is in the scale a person sees, so it is applied there and turned
            // back into an amplitude afterwards. Moving the amplitude by five points
            // instead would be a huge change when quiet and an inaudible one when loud.
            let wanted = (shown + step).clamp(0.0, 100.0);
            Property::new(
                pw::spa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(vec![
                    amplitude(wanted);
                    levels.channels.max(1)
                ])),
            )
        }
        Command::ToggleMute => {
            Property::new(pw::spa::sys::SPA_PROP_mute, Value::Bool(!levels.muted))
        }
    };

    // A hardware output is changed through the card's route, which is where its volume
    // lives; a virtual one has only the node.
    match sinks.route_of(sink) {
        Some((card, route)) => {
            let object = Value::Object(Object {
                type_: pw::spa::sys::SPA_TYPE_OBJECT_ParamRoute,
                id: pw::spa::sys::SPA_PARAM_Route,
                properties: vec![
                    Property::new(pw::spa::sys::SPA_PARAM_ROUTE_index, Value::Int(route.index)),
                    Property::new(
                        pw::spa::sys::SPA_PARAM_ROUTE_device,
                        Value::Int(sink.card.map(|(_, output)| output).unwrap_or(0)),
                    ),
                    Property::new(
                        pw::spa::sys::SPA_PARAM_ROUTE_props,
                        // The inner object is a Props, but it is carried inside a Route
                        // and is identified as part of one; a Props id here is quietly
                        // ignored by the server.
                        Value::Object(Object {
                            type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
                            id: pw::spa::sys::SPA_PARAM_Route,
                            properties: vec![property],
                        }),
                    ),
                    // Remembered across a reconnection, which is what a person means by
                    // having set the volume.
                    Property::new(pw::spa::sys::SPA_PARAM_ROUTE_save, Value::Bool(true)),
                ],
            });
            if let Some(pod) = serialize(&object) {
                card.device.set_param(ParamType::Route, 0, pod.as_pod());
            }
        }
        None => {
            let object = Value::Object(Object {
                type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
                id: pw::spa::sys::SPA_PARAM_Props,
                properties: vec![property],
            });
            if let Some(pod) = serialize(&object) {
                sink.node.set_param(ParamType::Props, 0, pod.as_pod());
            }
        }
    }
}

/// A serialised pod, kept alive while the call that uses it borrows from it.
struct Serialized(Vec<u8>);

impl Serialized {
    fn as_pod(&self) -> &Pod {
        Pod::from_bytes(&self.0).expect("what was just serialised is a pod")
    }
}

fn serialize(object: &Value) -> Option<Serialized> {
    let (bytes, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), object).ok()?;
    Some(Serialized(bytes.into_inner()))
}

/// PipeWire keeps an amplitude, and a person hears a scale.
///
/// Doubling the amplitude does not sound twice as loud, so every volume control there has
/// ever been is cubic. `wpctl` and `pactl` both show the cube root, and a bar that showed
/// the amplitude would read 9% where every other tool on the machine says 45%.
fn percent(amplitude: f32) -> f64 {
    (amplitude.max(0.0) as f64).cbrt() * 100.0
}

/// The amplitude that reads as this percentage, which is what PipeWire has to be given.
fn amplitude(percent: f64) -> f32 {
    let share = (percent / 100.0).max(0.0);
    (share * share * share) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_volume_shown_is_the_one_every_other_tool_shows() {
        // What PipeWire holds when `wpctl` says 0.45 and `pactl` says 45%.
        assert!((percent(0.091125) - 45.0).abs() < 0.001);
        assert_eq!(percent(1.0), 100.0);
        assert_eq!(percent(0.0), 0.0);
    }

    #[test]
    fn a_step_is_taken_in_the_scale_a_person_sees() {
        // Scrolling from 45% by five points lands on 50%, whatever that is in amplitude.
        let landed = percent(amplitude(45.0 + 5.0));
        assert!((landed - 50.0).abs() < 0.001, "landed on {landed}");
    }

    #[test]
    fn what_is_shown_and_what_is_set_are_the_same_scale() {
        for shown in [0.0, 12.5, 45.0, 100.0] {
            let round_trip = percent(amplitude(shown));
            assert!(
                (round_trip - shown).abs() < 0.001,
                "{shown} became {round_trip}"
            );
        }
    }

    #[test]
    fn an_amplitude_over_one_is_still_a_volume() {
        // PipeWire allows amplification past 100%, and clamping it here would report a
        // volume the machine is not playing at.
        assert!(percent(2.0) > 100.0);
    }
    use super::port_of;

    /// The names come from real cards: a UCM profile puts the socket in the sink's own
    /// name, while a card with routes puts it in the route's.
    #[test]
    fn a_socket_is_recognised_however_the_card_spells_it() {
        for (name, want) in [
            ("analog-output-headphones", "headphones"),
            ("[Out] Headphones", "headphones"),
            (
                "alsa_output.pci-0000_06_00.6.HiFi__Headphones__sink",
                "headphones",
            ),
            ("analog-output-speaker", "speaker"),
            (
                "alsa_output.pci-0000_06_00.6.HiFi__Speaker__sink",
                "speaker",
            ),
            ("[Out] Speaker", "speaker"),
            ("hdmi-output-0", "hdmi"),
            ("analog-output-lineout", "line-out"),
            ("bluez_output.AC_12_2F_00_11_22.1", "bluetooth"),
        ] {
            assert_eq!(port_of(name), want, "for {name:?}");
        }
    }

    /// A headset over Bluetooth is worth calling headphones: what a person wants to see is
    /// that the sound is on their head, not which radio carried it there.
    #[test]
    fn a_bluetooth_headset_counts_as_headphones() {
        assert_eq!(port_of("bluez_output.XX.headset-head-unit"), "headphones");
    }

    /// A card nobody has taught us about keeps a word of its own rather than being called
    /// speakers and quietly showing the wrong icon.
    #[test]
    fn an_unknown_socket_is_not_guessed_at() {
        assert_eq!(port_of("some-vendor-thing"), "other");
        assert_eq!(port_of(""), "other");
    }
}
