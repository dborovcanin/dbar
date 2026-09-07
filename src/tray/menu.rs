//! The menu a tray item opens when it is asked to.
//!
//! The protocol is `com.canonical.dbusmenu`, and it is the whole reason a tray host has to
//! draw anything beyond an icon: most applications that put an icon on a bar keep all of
//! their commands in the menu behind it, and several - a network applet among them - offer
//! no other way in at all. Calling `ContextMenu` on the item is the cheap alternative and
//! it does not work: the applications that rely on this protocol do not implement that
//! method, because the host is expected to draw the menu itself.
//!
//! What is read here is a tree of rows. A row has a label, or is a separator, or opens a
//! submenu, and may carry a mark and a small picture. That is the whole vocabulary: there
//! is no layout to speak of, which is what keeps this a menu rather than the widget tree
//! dbar deliberately does not have.

use std::sync::Arc;

use crate::dbus::{Arg, Connection, Value};
use crate::icon::Raster;
use crate::tray::icon;

const MENU_INTERFACE: &str = "com.canonical.dbusmenu";

/// How deep a menu is read in one go.
///
/// The protocol takes -1 for "all of it", and several applications answer that with every
/// submenu they have. A depth is asked for instead so one enormous menu cannot be turned
/// into one enormous message; the submenus below it are read when they are opened.
const DEPTH: i32 = 3;

/// One row of a menu.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// What the application calls this row, and what is sent back when it is chosen.
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    /// A rule across the menu rather than something to choose.
    pub separator: bool,
    /// Whether this row is ticked, for the rows that can be.
    pub toggle: Option<bool>,
    /// Whether choosing this row opens another menu rather than doing something.
    pub submenu: bool,
    /// A small picture the application gave for this row.
    pub icon: Option<Arc<Raster>>,
    /// The rows of that submenu, as far as they were read.
    pub children: Vec<Row>,
}

impl Row {
    /// Whether this row does something when it is chosen.
    pub fn selectable(&self) -> bool {
        self.enabled && !self.separator
    }
}

/// Read a menu, or as much of it as `DEPTH` reaches.
pub fn layout(bus: &mut Connection, service: &str, path: &str, size: u32) -> Option<Vec<Row>> {
    read_from(bus, service, path, 0, size)
}

/// Read one submenu, which is the same call aimed at a row rather than the root.
pub fn submenu(
    bus: &mut Connection,
    service: &str,
    path: &str,
    parent: i32,
    size: u32,
) -> Option<Vec<Row>> {
    // Applications are told a menu is about to be shown so the ones that fill theirs in
    // lazily have somewhere to do it. The answer says whether it changed anything, which
    // does not matter here: the layout is read straight afterwards either way.
    let _ = bus.call(
        service,
        path,
        MENU_INTERFACE,
        "AboutToShow",
        &[Arg::I32(parent)],
    );
    read_from(bus, service, path, parent, size)
}

fn read_from(
    bus: &mut Connection,
    service: &str,
    path: &str,
    parent: i32,
    size: u32,
) -> Option<Vec<Row>> {
    let reply = bus
        .call(
            service,
            path,
            MENU_INTERFACE,
            "GetLayout",
            &[Arg::I32(parent), Arg::I32(DEPTH), Arg::Array("s", &[])],
        )
        .ok()?;
    // The answer is a revision nobody here needs, then the root of what was asked for.
    let root = reply.get(1)?;
    Some(rows_of(root, size))
}

/// The children of one node, which is what a menu actually is: the node itself is the
/// thing that was opened, not a row in its own menu.
fn rows_of(node: &Value, size: u32) -> Vec<Row> {
    let parts = node.items();
    let children = match parts.get(2) {
        Some(children) => children.items(),
        None => return Vec::new(),
    };
    children
        .iter()
        .filter_map(|child| row_of(child, size))
        .collect()
}

/// One row, or nothing if the application asked for it not to be shown.
fn row_of(node: &Value, size: u32) -> Option<Row> {
    let parts = node.items();
    let id = parts.first()?.as_int()? as i32;
    let properties = parts.get(1)?;

    let text = |key: &str| properties.get(key).and_then(Value::as_str);
    let flag = |key: &str, default: bool| match properties.get(key) {
        Some(Value::Bool(value)) => *value,
        _ => default,
    };

    if !flag("visible", true) {
        return None;
    }
    let separator = text("type") == Some("separator");
    // A row that toggles says which kind of mark it wears; the state is a third value for
    // "indeterminate", which is drawn the same as off.
    let toggle = text("toggle-type")
        .filter(|kind| !kind.is_empty())
        .map(|_| properties.get("toggle-state").and_then(Value::as_int) == Some(1));

    let icon = properties
        .get("icon-data")
        .and_then(Value::as_bytes)
        .and_then(|bytes| icon::from_png(bytes, size))
        .map(Arc::new);

    Some(Row {
        id,
        label: label_of(text("label").unwrap_or_default()),
        enabled: flag("enabled", true),
        separator,
        toggle,
        submenu: text("children-display") == Some("submenu"),
        icon,
        children: rows_of(node, size),
    })
}

/// A label as a person should read it.
///
/// The protocol carries the keyboard mnemonic in the text itself, as an underscore before
/// the letter that would be underlined. dbar has no keyboard menu, so the marker is
/// removed rather than drawn - `_Quit` is a menu that says Quit, not one that says _Quit.
fn label_of(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut characters = raw.chars();
    while let Some(character) = characters.next() {
        match character {
            // The marker itself goes; a doubled one is one real underscore, and the
            // letter it marked is still part of the word.
            '_' => match characters.next() {
                Some('_') => out.push('_'),
                Some(marked) => out.push(marked),
                None => {}
            },
            other => out.push(other),
        }
    }
    out
}

/// Tell the application a row was chosen.
pub fn clicked(bus: &mut Connection, service: &str, path: &str, id: i32) {
    // The event carries data nothing reads and a timestamp applications accept as zero.
    // What happens next arrives as a layout change or as nothing at all; either way the
    // answer is not something to wait for.
    if let Err(e) = bus.send(
        service,
        path,
        MENU_INTERFACE,
        "Event",
        &[
            Arg::I32(id),
            Arg::Str("clicked"),
            Arg::Var(&Arg::I32(0)),
            Arg::U32(0),
        ],
    ) {
        log::debug!("a menu choice did not reach {service}: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn property(key: &str, value: Value) -> (Value, Value) {
        (Value::Str(key.to_string()), value)
    }

    fn node(id: i32, properties: Vec<(Value, Value)>, children: Vec<Value>) -> Value {
        Value::Seq(vec![
            Value::Int(id as i64),
            Value::Map(properties),
            Value::Seq(children),
        ])
    }

    /// The shape nm-applet's menu actually arrives in: labels, a separator, a disabled
    /// heading, a row that toggles, and a submenu.
    fn a_real_menu() -> Value {
        node(
            0,
            vec![property("children-display", Value::Str("submenu".into()))],
            vec![
                node(
                    73,
                    vec![
                        property("label", Value::Str("Ethernet Network".into())),
                        property("enabled", Value::Bool(false)),
                    ],
                    vec![],
                ),
                node(
                    75,
                    vec![property("type", Value::Str("separator".into()))],
                    vec![],
                ),
                node(
                    76,
                    vec![
                        property("label", Value::Str("Enable _Networking".into())),
                        property("toggle-type", Value::Str("checkmark".into())),
                        property("toggle-state", Value::Int(1)),
                    ],
                    vec![],
                ),
                node(
                    80,
                    vec![
                        property("label", Value::Str("VPN Connections".into())),
                        property("children-display", Value::Str("submenu".into())),
                    ],
                    vec![node(
                        81,
                        vec![property("label", Value::Str("Configure".into()))],
                        vec![],
                    )],
                ),
                node(
                    90,
                    vec![
                        property("label", Value::Str("Hidden".into())),
                        property("visible", Value::Bool(false)),
                    ],
                    vec![],
                ),
            ],
        )
    }

    #[test]
    fn a_menu_reads_back_as_the_rows_it_describes() {
        let rows = rows_of(&a_real_menu(), 16);
        // The hidden row is not one of them.
        assert_eq!(rows.len(), 4);

        assert_eq!(rows[0].label, "Ethernet Network");
        assert!(!rows[0].enabled, "a heading is disabled");
        assert!(!rows[0].selectable());

        assert!(rows[1].separator);
        assert!(!rows[1].selectable(), "a rule is not something to choose");

        assert_eq!(rows[2].toggle, Some(true));
        assert!(rows[2].selectable());

        assert!(rows[3].submenu);
        assert_eq!(rows[3].children.len(), 1);
        assert_eq!(rows[3].children[0].label, "Configure");
    }

    /// The mnemonic marker is part of the label the protocol sends, and dbar has no
    /// keyboard menu to underline anything for.
    #[test]
    fn the_keyboard_marker_is_not_drawn() {
        assert_eq!(label_of("Enable _Networking"), "Enable Networking");
        assert_eq!(label_of("_Quit"), "Quit");
        // A real underscore is written doubled.
        assert_eq!(label_of("a__b"), "a_b");
        assert_eq!(label_of("nothing to strip"), "nothing to strip");
        assert_eq!(label_of(""), "");
    }

    /// An unticked toggle is still a toggle: the row has to keep its column so a menu of
    /// them does not jump about as they are switched.
    #[test]
    fn a_toggle_is_remembered_even_when_it_is_off() {
        let off = node(
            1,
            vec![
                property("label", Value::Str("Off".into())),
                property("toggle-type", Value::Str("checkmark".into())),
                property("toggle-state", Value::Int(0)),
            ],
            vec![],
        );
        let row = row_of(&off, 16).expect("a visible row");
        assert_eq!(row.toggle, Some(false));

        // A row that does not toggle has no mark at all, which is not the same as an
        // unticked one.
        let plain = node(
            2,
            vec![property("label", Value::Str("Plain".into()))],
            vec![],
        );
        assert_eq!(row_of(&plain, 16).expect("a visible row").toggle, None);
    }

    #[test]
    fn a_row_that_says_nothing_useful_is_still_a_row() {
        // Missing properties fall back rather than dropping the row: a separator often
        // carries nothing but its type, and a label is not required.
        let bare = node(7, vec![], vec![]);
        let row = row_of(&bare, 16).expect("a row with no properties");
        assert_eq!(row.id, 7);
        assert_eq!(row.label, "");
        assert!(row.enabled, "enabled unless the application says otherwise");
        assert!(!row.submenu);
    }

    #[test]
    fn nonsense_is_refused_rather_than_guessed_at() {
        assert_eq!(row_of(&Value::Int(3), 16), None);
        assert_eq!(row_of(&Value::Seq(vec![]), 16), None);
        assert!(rows_of(&Value::Int(3), 16).is_empty());
    }
}
