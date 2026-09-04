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
use pw::spa::pod::{Pod, Value, ValueArray};
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
];

/// Start listening to PipeWire, and report readings as they change.
pub fn spawn(sender: calloop::channel::Sender<Reading>) -> Result<()> {
    std::thread::Builder::new()
        .name("audio".to_string())
        .spawn(move || match run(sender) {
            Ok(()) => log::info!("PipeWire has gone; the volume module keeps its last reading"),
            Err(e) => log::warn!("volume is unavailable: {e:#}"),
        })
        .context("spawning the audio thread")?;
    Ok(())
}

/// The proxies and their listeners, kept alive for as long as the objects they stand for.
///
/// A proxy stops delivering events the moment it is dropped, so binding one and letting it
/// fall out of scope is the same as not binding it at all.
type Bound = Rc<
    RefCell<
        HashMap<
            u32,
            (
                Box<dyn pw::proxy::ProxyT>,
                Vec<Box<dyn pw::proxy::Listener>>,
            ),
        >,
    >,
>;

/// One output PipeWire is listing, and what it last said about itself.
#[derive(Default)]
struct Sink {
    name: String,
    description: Option<String>,
    volume: Option<f64>,
    muted: Option<bool>,
}

/// Everything the thread knows, shared between the callbacks PipeWire calls back into.
struct Sinks {
    /// The node the volume keys move, named by the "default" metadata.
    default: Option<String>,
    by_id: HashMap<u32, Sink>,
    sender: calloop::channel::Sender<Reading>,
    /// The last reading sent, so an event that changes nothing does not redraw the bar.
    last: Option<(f64, bool, Option<String>)>,
}

impl Sinks {
    /// Send what the default sink is now saying, if it is saying anything new.
    fn publish(&mut self) {
        let Some(sink) = self
            .default
            .as_ref()
            .and_then(|name| self.by_id.values().find(|sink| &sink.name == name))
        else {
            return;
        };
        let (Some(volume), Some(muted)) = (sink.volume, sink.muted) else {
            return;
        };
        let current = (volume, muted, sink.description.clone());
        if self.last.as_ref() == Some(&current) {
            return;
        }
        self.last = Some(current);
        log::debug!("volume {volume:.1}%, muted {muted}");

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
            match &sink.description {
                Some(description) => Field::Text(description.clone()),
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

fn run(sender: calloop::channel::Sender<Reading>) -> Result<()> {
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
            if sinks.by_id.remove(&id).is_some() {
                sinks.publish();
            }
        }
    };

    let _listener = registry
        .add_listener_local()
        .global(on_global)
        .global_remove(on_remove)
        .register();

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
    sinks.borrow_mut().by_id.insert(
        id,
        Sink {
            name: name.to_string(),
            description: props
                .and_then(|p| p.get("node.description"))
                .map(str::to_string),
            ..Sink::default()
        },
    );

    let listener = {
        let sinks = sinks.clone();
        node.add_listener_local()
            .param(move |_seq, _type, _index, _next, param| {
                let Some(param) = param else { return };
                let Some((volume, muted)) = volume_of(param) else {
                    return;
                };
                let mut sinks = sinks.borrow_mut();
                if let Some(sink) = sinks.by_id.get_mut(&id) {
                    sink.volume = Some(volume);
                    sink.muted = Some(muted);
                }
                sinks.publish();
            })
            .register()
    };
    // Asking to be told is not the same as being told once: the first values arrive
    // because the subscription itself replays them.
    node.subscribe_params(&[ParamType::Props]);

    proxies
        .borrow_mut()
        .insert(id, (Box::new(node), vec![Box::new(listener)]));
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

    proxies
        .borrow_mut()
        .insert(global.id, (Box::new(metadata), vec![Box::new(listener)]));
}

/// The volume and mute a `Props` parameter carries, if it carries them.
fn volume_of(param: &Pod) -> Option<(f64, bool)> {
    let (_, value) = PodDeserializer::deserialize_any_from(param.as_bytes()).ok()?;
    let Value::Object(object) = value else {
        return None;
    };

    let mut volume = None;
    let mut muted = None;
    for property in &object.properties {
        match (property.key, &property.value) {
            (pw::spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(v))) => {
                // Every channel has its own volume and a person has one. The loudest is
                // what they would call the volume; the mean would report a muted right
                // channel as half.
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
    Some((volume?, muted?))
}

/// PipeWire keeps an amplitude, and a person hears a scale.
///
/// Doubling the amplitude does not sound twice as loud, so every volume control there has
/// ever been is cubic. `wpctl` and `pactl` both show the cube root, and a bar that showed
/// the amplitude would read 9% where every other tool on the machine says 45%.
fn percent(amplitude: f32) -> f64 {
    (amplitude.max(0.0) as f64).cbrt() * 100.0
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
    fn an_amplitude_over_one_is_still_a_volume() {
        // PipeWire allows amplification past 100%, and clamping it here would report a
        // volume the machine is not playing at.
        assert!(percent(2.0) > 100.0);
    }
}
