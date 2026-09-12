use super::*;
use crate::collect::{Reading, Which};
use crate::status::{ActionTarget, Fields, State, StatusItem, Unit, Value};

/// A text backend with no fonts: every character is one unit wide.
///
/// Layout only ever asks how wide a string is, so a fixed width makes every expected
/// number in these tests a character count.
struct Fixed;

impl Measure for Fixed {
    fn measure(&mut self, text: &str) -> f32 {
        text.chars().count() as f32
    }
}

fn item(id: &str, text: &str) -> StatusItem {
    let mut fields = Fields::default();
    fields.set("text", Value::Text(text.to_string()));
    StatusItem {
        id: Some(id.to_string()),
        fields,
        state: State::Idle,
        urgent: false,
        foreground: None,
        background: None,
        action: None,
    }
}

fn with_percent(mut item: StatusItem, percent: f64) -> StatusItem {
    item.fields.set(
        "percent",
        Value::Num {
            v: percent,
            unit: Unit::Percent,
        },
    );
    item.fields.set_primary("percent");
    item
}

fn frame_of(config: &str, items: &[StatusItem]) -> Frame {
    frame_with(config, items, Registry::new(&Default::default()))
}

fn frame_with(config: &str, items: &[StatusItem], native: Registry) -> Frame {
    frame_showing(config, items, native, &Default::default())
}

fn frame_showing(
    config: &str,
    items: &[StatusItem],
    native: Registry,
    alt: &std::collections::HashMap<String, usize>,
) -> Frame {
    frame_folded(config, items, native, alt, &Default::default())
}

fn frame_folded(
    config: &str,
    items: &[StatusItem],
    native: Registry,
    alt: &std::collections::HashMap<String, usize>,
    collapsed: &std::collections::HashSet<String>,
) -> Frame {
    frame_paged(config, items, native, alt, collapsed, &Default::default())
}

fn frame_paged(
    config: &str,
    items: &[StatusItem],
    native: Registry,
    alt: &std::collections::HashMap<String, usize>,
    collapsed: &std::collections::HashSet<String>,
    pages: &std::collections::HashMap<String, usize>,
) -> Frame {
    frame_waiting(
        config,
        items,
        native,
        alt,
        collapsed,
        pages,
        &Default::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn frame_waiting(
    config: &str,
    items: &[StatusItem],
    native: Registry,
    alt: &std::collections::HashMap<String, usize>,
    collapsed: &std::collections::HashSet<String>,
    pages: &std::collections::HashMap<String, usize>,
    waiting: &std::collections::HashSet<Which>,
) -> Frame {
    let cfg = Config::parse(config).expect("test config parses");
    let inputs = Inputs {
        items,
        native: &native,
        sway: &SwayState::default(),
        alt,
        pages,
        collapsed,
        collapsed_groups: &Default::default(),
        switching: &Default::default(),
        folding: &Default::default(),
        module_folding: &Default::default(),
        waiting,
        spin: 3,
        tray: &Default::default(),
        output: None,
    };
    compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None)
}

const BASIC: &str = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu", "mem"]

[module.cpu]
padding = 0

[module.mem]
padding = 0
"##;

#[test]
fn a_language_module_says_what_the_config_calls_the_layout() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["lang"]

[module.lang]
source = "sway:language"
padding = 0

[module.lang.layouts]
"English (US)" = "EN"
"##;
    let cfg = Config::parse(config).expect("test config parses");
    let mut sway = SwayState::default();
    let render = |sway: &SwayState| {
        let inputs = Inputs {
            items: &[],
            native: &Registry::new(&Default::default()),
            sway,
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed_groups: &Default::default(),
            switching: &Default::default(),
            folding: &Default::default(),
            module_folding: &Default::default(),
            collapsed: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: None,
        };
        let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
        frame.groups.first().map(|g| g.modules[0].text.clone())
    };

    // Nothing to show before the compositor has said anything, the same as a collector
    // that has not read yet.
    assert_eq!(render(&sway), None);

    sway.layout = Some(crate::sway::Layout {
        name: "English (US)".to_string(),
        index: 0,
    });
    assert_eq!(render(&sway).as_deref(), Some(" EN "));

    // A layout the config does not name is abbreviated rather than left out.
    sway.layout = Some(crate::sway::Layout {
        name: "Serbian".to_string(),
        index: 1,
    });
    assert_eq!(render(&sway).as_deref(), Some(" SE "));
}

/// A bar exists once per screen, so its workspace list is about that screen. Listing
/// all of them would put the other monitor's workspaces on this one, which is the whole
/// thing multiple bars are for.
#[test]
fn a_workspace_list_is_about_the_screen_its_bar_is_on() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws"]

[module.ws]
source = "sway:workspaces"
padding = 0
"##;
    let cfg = Config::parse(config).expect("test config parses");
    let sway = two_screens();
    let on = |output: Option<&str>| {
        let inputs = Inputs {
            items: &[],
            native: &Registry::new(&Default::default()),
            sway: &sway,
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed_groups: &Default::default(),
            switching: &Default::default(),
            folding: &Default::default(),
            module_folding: &Default::default(),
            collapsed: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output,
        };
        let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
        let modules = frame.groups.first().map(|g| g.modules.clone());
        modules
            .unwrap_or_default()
            .iter()
            .map(|m| m.text.clone())
            .collect::<Vec<_>>()
    };

    assert_eq!(on(Some("DP-1")), ["1", "3"]);
    assert_eq!(on(Some("HDMI-A-1")), ["2"]);
    // A bar that has not been told which screen it is on shows the lot, because half a
    // list is worse than a whole one.
    assert_eq!(on(None), ["1", "2", "3"]);
}

/// One frame of a bar whose only module is `sway:workspaces`, on the two screens the
/// other compositor tests use.
fn workspaces(
    config: &str,
    switching: &std::collections::HashMap<String, Leaving>,
    collapsed: &std::collections::HashSet<String>,
) -> Frame {
    let cfg = Config::parse(config).expect("test config parses");
    let sway = two_screens();
    let inputs = Inputs {
        items: &[],
        native: &Registry::new(&Default::default()),
        sway: &sway,
        alt: &Default::default(),
        pages: &Default::default(),
        collapsed_groups: &Default::default(),
        switching,
        folding: &Default::default(),
        module_folding: &Default::default(),
        collapsed,
        waiting: &Default::default(),
        spin: 0,
        tray: &Default::default(),
        output: None,
    };
    compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None)
}

fn wordings(frame: &Frame) -> Vec<&str> {
    frame.groups[0]
        .modules
        .iter()
        .map(|module| module.text.as_str())
        .collect()
}

#[test]
fn workspace_icons_can_be_native_or_text_and_are_optional() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws"]

[module.ws]
source = "sway:workspaces"
format = "$name"
icons = { "1" = "$slack", "2" = "󰘦", "3" = "$chrome" }
padding = 0
"##;
    let frame = workspaces(config, &Default::default(), &Default::default());
    assert_eq!(wordings(&frame), ["1", "2 󰘦", "3"]);
    let modules = &frame.groups[0].modules;
    assert_eq!(
        modules[0].icon.as_ref().map(|icon| icon.icon),
        Some(Icon::Slack)
    );
    assert!(modules[0].icon.as_ref().unwrap().x > modules[0].text_x);
    assert!(modules[1].icon.is_none());
    assert_eq!(
        modules[2].icon.as_ref().map(|icon| icon.icon),
        Some(Icon::Chrome)
    );
}

/// A bar showing icons and nothing else is what `icons` with no wording is for, and a
/// workspace icon written as text has to start where a native one would.
#[test]
fn a_workspace_icon_needs_no_wording_in_front_of_it() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws"]

[module.ws]
source = "sway:workspaces"
format = ""
icons = { "1" = "X", "2" = "$slack" }
padding = 0
"##;
    let frame = workspaces(config, &Default::default(), &Default::default());
    assert_eq!(
        wordings(&frame),
        ["X", ""],
        "no wording, so no gap before one"
    );
    let modules = &frame.groups[0].modules;
    assert_eq!(
        modules.len(),
        2,
        "the third workspace is named by no icon and says nothing, so it is not there"
    );
    assert_eq!(modules[0].width, 1.0, "one character and no gap");
    assert_eq!(
        modules[1].icon.as_ref().map(|icon| icon.icon),
        Some(Icon::Slack)
    );
}

/// A click between two wordings measures the one it is leaving, and the icon belongs to
/// the workspace rather than to either wording, so both of them have to carry it.
#[test]
fn a_workspace_icon_is_on_the_wording_a_click_is_leaving_too() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws"]

[module.ws]
source = "sway:workspaces"
format = "$name"
format_alt = "w$name"
icons = { "1" = "XX" }
padding = 0
"##;
    let switching =
        std::collections::HashMap::from([("ws".to_string(), Leaving { from: 1, at: 0.0 })]);
    let frame = workspaces(config, &switching, &Default::default());
    let travelling = &frame.groups[0].modules[0];
    assert_eq!(travelling.text, "1 XX");
    assert_eq!(
        travelling.width, 5.0,
        "the box it is leaving is `w1 XX`, icon included, so the icon is not cut off"
    );
}

/// Folding takes the wording and leaves the icon, and a workspace icon is the icon.
#[test]
fn a_folded_workspace_is_left_with_the_icon_it_was_given() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws"]

[module.ws]
source = "sway:workspaces"
format = "$name"
icons = { "1" = "$slack", "2" = "XX" }
icon = "$cpu"
icon_size = 10
collapsible = true
padding = 0
"##;
    let open = workspaces(config, &Default::default(), &Default::default());
    assert_eq!(wordings(&open), ["1", "2 XX", "3"]);
    assert_eq!(
        open.groups[0].modules[2].icon.as_ref().map(|i| i.icon),
        None,
        "a workspace `icons` passes over gets nothing, not the style's icon on the \
             other side of its name"
    );

    let folded = workspaces(
        config,
        &Default::default(),
        &std::collections::HashSet::from(["ws".to_string()]),
    );
    assert_eq!(wordings(&folded), ["", "XX", ""]);
    let modules = &folded.groups[0].modules;
    assert_eq!(
        modules[0].icon.as_ref().map(|i| i.icon),
        Some(Icon::Slack),
        "the workspace's own icon is what is left to click on"
    );
    assert!(modules[1].icon.is_none(), "its icon is the text it is now");
    assert_eq!(
        modules[2].icon.as_ref().map(|i| i.icon),
        Some(Icon::Cpu),
        "folded down, a workspace with no icon of its own falls back on the style's"
    );
}

/// `scope = "session"` is what a single-screen configuration always had, and what
/// someone who wants every workspace on every bar asks for.
#[test]
fn a_workspace_list_can_be_asked_for_the_whole_session() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws"]

[module.ws]
source = "sway:workspaces"
scope = "session"
padding = 0
"##;
    let cfg = Config::parse(config).expect("test config parses");
    let sway = two_screens();
    let inputs = Inputs {
        items: &[],
        native: &Registry::new(&Default::default()),
        sway: &sway,
        alt: &Default::default(),
        pages: &Default::default(),
        collapsed_groups: &Default::default(),
        switching: &Default::default(),
        folding: &Default::default(),
        module_folding: &Default::default(),
        collapsed: &Default::default(),
        waiting: &Default::default(),
        spin: 0,
        tray: &Default::default(),
        output: Some("HDMI-A-1"),
    };
    let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
    let texts: Vec<String> = frame.groups[0]
        .modules
        .iter()
        .map(|m| m.text.clone())
        .collect();
    assert_eq!(texts, ["1", "2", "3"]);
}

/// The same for the title above it: only one window in the session has focus, and the
/// other screen is still showing something.
#[test]
fn a_window_module_says_what_its_own_screen_is_showing() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["win"]

[module.win]
source = "sway:window"
padding = 0
"##;
    let cfg = Config::parse(config).expect("test config parses");
    let sway = two_screens();
    let on = |output: Option<&str>| {
        let inputs = Inputs {
            items: &[],
            native: &Registry::new(&Default::default()),
            sway: &sway,
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed_groups: &Default::default(),
            switching: &Default::default(),
            folding: &Default::default(),
            module_folding: &Default::default(),
            collapsed: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output,
        };
        let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
        frame.groups.first().map(|g| g.modules[0].text.clone())
    };

    assert_eq!(on(Some("DP-1")).as_deref(), Some("vim"));
    assert_eq!(on(Some("HDMI-A-1")).as_deref(), Some("a page"));
    // Nothing said about the screen: the one with the focus is the best guess there is.
    assert_eq!(on(None).as_deref(), Some("vim"));
}

/// Two screens, the keyboard on the first, and a workspace on each plus one more that
/// is open but not on screen.
fn two_screens() -> SwayState {
    let workspace =
        |name: &str, output: &str, focused: bool, visible: bool| crate::sway::Workspace {
            name: name.to_string(),
            output: output.to_string(),
            focused,
            visible,
            urgent: false,
        };
    SwayState {
        workspaces: vec![
            workspace("1", "DP-1", true, true),
            workspace("2", "HDMI-A-1", false, true),
            workspace("3", "DP-1", false, false),
        ],
        windows: [("DP-1", "vim", "foot"), ("HDMI-A-1", "a page", "firefox")]
            .into_iter()
            .map(|(output, title, app_id)| {
                (
                    output.to_string(),
                    crate::sway::Window {
                        title: title.to_string(),
                        app_id: app_id.to_string(),
                        class: String::new(),
                    },
                )
            })
            .collect(),
        focused_output: Some("DP-1".to_string()),
        ..SwayState::default()
    }
}

/// A window module can be written against what the window is, not only what it is
/// showing: a rule that gives one program its own colour must not key on a title that
/// changes with every tab.
#[test]
fn a_window_module_can_name_what_the_window_is() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["title"]

[module.title]
source = "sway:window"
format = "$app_id|$class|'?': $title"
"##;
    let cfg = Config::parse(config).expect("test config parses");
    let sway = two_screens();
    let inputs = Inputs {
        items: &[],
        native: &Registry::new(&Default::default()),
        sway: &sway,
        alt: &Default::default(),
        pages: &Default::default(),
        collapsed_groups: &Default::default(),
        switching: &Default::default(),
        folding: &Default::default(),
        module_folding: &Default::default(),
        collapsed: &Default::default(),
        waiting: &Default::default(),
        spin: 0,
        tray: &Default::default(),
        output: Some("HDMI-A-1"),
    };
    let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
    assert_eq!(frame.groups[0].modules[0].text, "firefox: a page");
}

/// One module, one icon per application - the same expansion a workspace list gets.
#[test]
fn a_tray_module_becomes_one_module_per_application() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["tray"]

[module.tray]
source = "tray"
padding = 0
"##;
    let cfg = Config::parse(config).expect("test config parses");
    let tray = crate::tray::TrayState {
        items: vec![tray_item("a", "One"), tray_item("b", "Two")],
    };
    let frame = render_with(&cfg, &tray);
    assert_eq!(frame.groups[0].modules.len(), 2);
    // Each carries its own artwork and its own click target rather than the module's.
    for module in &frame.groups[0].modules {
        assert!(module.icon.as_ref().is_some_and(|i| i.art.is_some()));
    }
    assert!(matches!(
        frame.groups[0].modules[0].action,
        Some(ActionTarget::Tray { .. })
    ));
}

/// A tray item says nothing by default - the picture is the whole module - and the
/// check that drops a module with no text would otherwise drop every one of them.
#[test]
fn an_item_with_no_wording_is_still_drawn() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["tray"]

[module.tray]
source = "tray"
padding = 0
"##;
    let cfg = Config::parse(config).expect("test config parses");
    let tray = crate::tray::TrayState {
        items: vec![tray_item("a", "One")],
    };
    let frame = render_with(&cfg, &tray);
    assert_eq!(frame.groups[0].modules[0].text, "");
    assert!(frame.groups[0].modules[0].icon.is_some());
}

/// An item that brought no icon at all and has nothing written on it is nothing to
/// draw, and a rectangle of empty bar is worse than no rectangle.
#[test]
fn an_item_with_neither_icon_nor_wording_is_left_out() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["tray"]

[module.tray]
source = "tray"
padding = 0
"##;
    let cfg = Config::parse(config).expect("test config parses");
    let mut item = tray_item("a", "One");
    item.icon = None;
    let tray = crate::tray::TrayState { items: vec![item] };
    assert!(render_with(&cfg, &tray).groups.is_empty());
}

/// Items arrive as their applications start, so a bar that has been up since morning
/// is in one order and the same bar restarted at lunch is in another. Naming the ones
/// that matter pins them; the rest keep arriving after them.
#[test]
fn a_tray_can_be_told_what_to_show_and_in_what_order() {
    let config = |body: &str| {
        format!(
            r##"
[left]
groups = ["g"]

[group.g]
modules = ["tray"]

[module.tray]
source = "tray"
format = "$id"
padding = 0
{body}
"##
        )
    };
    let mut quiet = tray_item("k3", "Quiet");
    quiet.status = crate::tray::Status::Passive;
    let tray = crate::tray::TrayState {
        items: vec![tray_item("k1", "Volume"), tray_item("k2", "Network"), quiet],
    };
    let drawn = |body: &str| {
        let cfg = Config::parse(&config(body)).expect("test config parses");
        render_with(&cfg, &tray)
            .groups
            .first()
            .map(|g| g.modules.iter().map(|m| m.text.clone()).collect::<Vec<_>>())
            .unwrap_or_default()
    };

    // As they arrived, passive ones included, which is what a bar without either key
    // has always drawn.
    assert_eq!(drawn(""), ["volume", "network", "quiet"]);
    assert_eq!(drawn("show_passive = false"), ["volume", "network"]);
    // Named first, in the order named; everything else after, in arrival order.
    assert_eq!(
        drawn("order = [\"network\"]"),
        ["network", "volume", "quiet"]
    );
    // An id nothing in the tray has costs nothing.
    assert_eq!(
        drawn("order = [\"nothing\", \"quiet\"]"),
        ["quiet", "volume", "network"]
    );
}

fn tray_item(key: &str, title: &str) -> crate::tray::Item {
    crate::tray::Item {
        key: key.to_string(),
        is_menu: false,
        has_menu: false,
        id: title.to_lowercase(),
        title: title.to_string(),
        status: crate::tray::Status::Active,
        icon: Some(Arc::new(crate::icon::Raster {
            width: 1,
            height: 1,
            pixels: vec![255, 255, 255, 255],
        })),
    }
}

fn render_with(cfg: &Config, tray: &crate::tray::TrayState) -> Frame {
    let inputs = Inputs {
        items: &[],
        native: &Registry::new(&Default::default()),
        sway: &SwayState::default(),
        alt: &Default::default(),
        pages: &Default::default(),
        collapsed_groups: &Default::default(),
        switching: &Default::default(),
        folding: &Default::default(),
        module_folding: &Default::default(),
        collapsed: &Default::default(),
        waiting: &Default::default(),
        spin: 0,
        tray,
        output: None,
    };
    compute(cfg, &inputs, 200.0, 10.0, &mut Fixed, None)
}

fn menu_row(label: &str) -> crate::tray::menu::Row {
    crate::tray::menu::Row {
        id: 1,
        label: label.to_string(),
        enabled: true,
        ..Default::default()
    }
}

fn menu_style() -> crate::config::Menu {
    Config::parse("[bar]\nheight = 20\n")
        .expect("a bar with no modules is a config")
        .menu
}

/// A menu row's label comes from the application, and the popup has a width the
/// config decided. A label longer than that used to be shaped whole and then drawn
/// straight off the edge.
#[test]
fn a_menu_label_longer_than_the_menu_is_cut_to_fit() {
    let style = menu_style();
    let rows = vec![menu_row(&"label ".repeat(4096))];
    let frame = menu(&rows, &style, 16.0, 10.0, 5.0, None, &mut Fixed);
    assert!(
        frame.width <= style.max_width,
        "the menu is {} wide against a limit of {}",
        frame.width,
        style.max_width
    );
    let drawn = &frame.rows[0].text;
    assert!(drawn.ends_with(ELLIPSIS), "{drawn:?} was not cut");
    assert!(
        (drawn.chars().count() as f32) < style.max_width,
        "the label is still {} characters against a menu {} wide",
        drawn.chars().count(),
        style.max_width
    );
}

/// A menu is as wide as the longest thing it has to say, and every row is the same
/// height so the pointer does not have to hunt for them.
#[test]
fn a_menu_is_sized_by_what_it_has_to_say() {
    let rows = vec![menu_row("Short"), menu_row("A much longer label")];
    let frame = menu(&rows, &menu_style(), 16.0, 10.0, 5.0, None, &mut Fixed);
    // The stub measurer makes every character one unit wide.
    assert!(frame.width > "A much longer label".len() as f32);
    assert_eq!(frame.rows.len(), 2);
    assert_eq!(frame.rows[0].height, frame.rows[1].height);
    // Rows follow one another with no gap, and the whole is as tall as they are plus
    // the padding at each end.
    assert_eq!(frame.rows[1].y, frame.rows[0].y + frame.rows[0].height);
    assert!(frame.height > frame.rows[1].y + frame.rows[1].height);
}

/// A rule is shorter than a row and cannot be pointed at: hunting for a row and
/// landing on the line between two of them is how a menu feels broken.
#[test]
fn a_separator_is_not_something_the_pointer_can_land_on() {
    let rows = vec![
        menu_row("One"),
        crate::tray::menu::Row {
            id: 2,
            separator: true,
            ..Default::default()
        },
        menu_row("Two"),
    ];
    let frame = menu(&rows, &menu_style(), 16.0, 10.0, 5.0, None, &mut Fixed);
    assert!(frame.rows[1].height < frame.rows[0].height);

    let middle = |index: usize| frame.rows[index].y + frame.rows[index].height / 2.0;
    assert_eq!(frame.row_at(middle(0)), Some(0));
    assert_eq!(frame.row_at(middle(1)), None, "a rule is not a row");
    assert_eq!(frame.row_at(middle(2)), Some(2));
}

/// The highlight follows the pointer, and never onto a row that does nothing.
#[test]
fn only_a_row_that_can_be_chosen_is_highlighted() {
    let rows = vec![
        menu_row("Enabled"),
        crate::tray::menu::Row {
            id: 2,
            label: "Disabled".to_string(),
            enabled: false,
            ..Default::default()
        },
    ];
    let style = menu_style();
    let frame = menu(&rows, &style, 16.0, 10.0, 5.0, Some(0), &mut Fixed);
    assert!(frame.rows[0].highlight);
    assert!(!frame.rows[1].highlight);

    // Pointing at the disabled row highlights nothing, and it keeps its quieter ink.
    let frame = menu(&rows, &style, 16.0, 10.0, 5.0, Some(1), &mut Fixed);
    assert!(!frame.rows[0].highlight);
    assert!(!frame.rows[1].highlight);
    assert_eq!(frame.rows[1].foreground, style.disabled);
}

/// Both columns exist on every row so the labels line up, whether or not each row has
/// something to put in them.
#[test]
fn a_mark_and_an_arrow_are_placed_only_where_they_belong() {
    let rows = vec![
        crate::tray::menu::Row {
            id: 1,
            label: "Ticked".to_string(),
            enabled: true,
            toggle: Some(true),
            ..Default::default()
        },
        crate::tray::menu::Row {
            id: 2,
            label: "Unticked".to_string(),
            enabled: true,
            toggle: Some(false),
            ..Default::default()
        },
        crate::tray::menu::Row {
            id: 3,
            label: "More".to_string(),
            enabled: true,
            submenu: true,
            ..Default::default()
        },
    ];
    let frame = menu(&rows, &menu_style(), 16.0, 10.0, 5.0, None, &mut Fixed);
    assert!(frame.rows[0].mark.is_some());
    assert!(
        frame.rows[1].mark.is_none(),
        "an unticked row wears no tick"
    );
    assert!(frame.rows[2].arrow.is_some());
    assert!(frame.rows[0].arrow.is_none());
    // The labels start in the same place regardless.
    assert_eq!(frame.rows[0].text_x, frame.rows[2].text_x);
}

/// What a module is asked to draw is somebody else's: a window title, a line from a
/// script. Finding the few characters that fit must not cost what the sender sent, in
/// shaping or in what the cache then holds on to.
#[test]
fn a_very_long_line_is_not_measured_end_to_end() {
    /// Records the longest string it was ever asked about.
    struct Longest(usize);
    impl Measure for Longest {
        fn measure(&mut self, text: &str) -> f32 {
            self.0 = self.0.max(text.chars().count());
            text.chars().count() as f32
        }
    }

    let long = "x".repeat(256 * 1024);
    let mut measure = Longest(0);
    let shown = truncate(&long, 20.0, &mut measure);
    assert_eq!(shown.chars().count(), 20, "19 characters and an ellipsis");
    assert!(
        measure.0 <= 128,
        "measured a string of {} characters to draw 20 of them",
        measure.0
    );

    // Text that takes up no room at all never overflows a budget, so the growing
    // window would grow to whatever arrived. Zero-width spaces are the plain case,
    // and a joined emoji sequence is the one that turns up by accident.
    struct Weightless(usize);
    impl Measure for Weightless {
        fn measure(&mut self, text: &str) -> f32 {
            self.0 = self.0.max(text.chars().count());
            0.0
        }
    }
    let empty_looking = "\u{200b}".repeat(64 * 1024);
    let mut measure = Weightless(0);
    truncate(&empty_looking, 20.0, &mut measure);
    assert!(
        measure.0 <= MOST_SHAPED,
        "shaped {} characters of text that is no width at all",
        measure.0
    );

    // A line that fits is still handed back whole, however close to the budget it is.
    let mut measure = Longest(0);
    assert_eq!(truncate("short", 20.0, &mut measure), "short");
    assert_eq!(
        truncate("exactly twenty chars", 20.0, &mut measure),
        "exactly twenty chars"
    );
}

/// The bar keeps its ground while it is saying what went wrong. The renderer paints
/// what the frame carries and nothing else, so a fault frame that carried no ground
/// put its message straight onto the wallpaper.
#[test]
fn a_fault_keeps_the_bar_it_is_drawn_on() {
    let cfg = Config::parse(
        "[colors]\nink = \"#1e1e2e\"\n\n[bar]\nheight = 20\n\n\
             [bar.background]\ncolor = \"$ink\"\nradius = 8\n",
    )
    .expect("a bar with a ground");
    let frame = fault(&cfg, "the provider stopped", 200.0, 20.0, &mut Fixed);
    assert_eq!(frame.background, cfg.bar.background);
    assert_eq!(frame.radius, cfg.bar.radius);
    assert_eq!(frame.groups[0].modules[0].text, "the provider stopped");

    // A bar configured with no ground of its own still has none here.
    let plain = Config::parse("[bar]\nheight = 20\n").expect("a bar with nothing on it");
    let frame = fault(&plain, "the provider stopped", 200.0, 20.0, &mut Fixed);
    assert_eq!(frame.background, Color::TRANSPARENT);
}

/// The first frame has nothing on screen to compare against, so all of it is new.
#[test]
fn the_first_frame_damages_everything() {
    let frame = frame_of(BASIC, &[item("cpu", "1%"), item("mem", "2%")]);
    assert_eq!(frame.damage(&Frame::default()), Damage::All);
}

/// A frame that draws the same thing damages nothing. This is the case that matters:
/// a collector that read the same value again must not make the compositor take the
/// bar back.
#[test]
fn a_frame_that_changed_nothing_damages_nothing() {
    let items = [item("cpu", "1%"), item("mem", "2%")];
    let one = frame_of(BASIC, &items);
    let two = frame_of(BASIC, &items);
    assert_eq!(two.damage(&one), Damage::Rects(Vec::new()));
}

/// The same layout on a different surface is not the same picture: the buffer is a new
/// size, or the same size at another scale, and none of it has been painted. Comparing
/// the layouts alone would attach a buffer and tell the compositor to look at none of
/// it, which is a bar that stops updating until something moves.
#[test]
fn a_frame_on_a_surface_that_changed_damages_everything() {
    let items = [item("cpu", "1%"), item("mem", "2%")];
    let one = frame_of(BASIC, &items);
    let two = frame_of(BASIC, &items);
    let surface = (1920, 30, 1);

    assert_eq!(
        two.damage_since(&one, Some(surface), surface),
        Damage::Rects(Vec::new()),
        "the same frame on the same surface is still nothing to repaint"
    );
    // A monitor whose scale changed lays the bar out identically and shares not one
    // pixel with what was there.
    assert_eq!(
        two.damage_since(&one, Some(surface), (1920, 30, 2)),
        Damage::All
    );
    assert_eq!(
        two.damage_since(&one, Some(surface), (3840, 30, 1)),
        Damage::All
    );
    // Nothing has reached the screen yet: the first frame, and every frame after one
    // that failed on its way there.
    assert_eq!(two.damage_since(&one, None, surface), Damage::All);
}

/// A module whose wording changed damages the island holding it, and nothing else.
#[test]
fn a_changed_module_damages_its_island() {
    let before = frame_of(BASIC, &[item("cpu", "1%"), item("mem", "2%")]);
    let after = frame_of(BASIC, &[item("cpu", "99%"), item("mem", "2%")]);
    let Damage::Rects(rects) = after.damage(&before) else {
        panic!("a wording change is not the whole bar");
    };
    assert!(!rects.is_empty(), "something did change");
    // Every rectangle is one of the group's, before or after.
    let group = &after.groups[0];
    for (x, _, w, _) in &rects {
        assert!(
            *x >= group.x - 1.0 && x + w <= group.x + group.width + 1.0,
            "damage {x}+{w} reaches outside the island at {}+{}",
            group.x,
            group.width
        );
    }
}

/// A module that grows moves everything after it, and what moved has to be repaired
/// where it used to be as well as drawn where it is now.
#[test]
fn a_module_that_grows_damages_what_it_pushed_along() {
    let config = r##"
[left]
groups = ["one", "two"]

[group.one]
modules = ["cpu"]

[group.two]
modules = ["mem"]

[module.cpu]
padding = 0

[module.mem]
padding = 0
"##;
    let narrow = frame_of(config, &[item("cpu", "1%"), item("mem", "2%")]);
    let wide = frame_of(config, &[item("cpu", "1000000%"), item("mem", "2%")]);
    let Damage::Rects(rects) = wide.damage(&narrow) else {
        panic!("two islands are not the whole bar");
    };
    // The island that grew and the one it shoved along, each named twice - where it
    // was and where it is.
    assert_eq!(rects.len(), 4, "both islands, before and after: {rects:?}");
    let widest = rects.iter().map(|(_, _, w, _)| *w).fold(0.0f32, f32::max);
    assert!(
        widest >= wide.groups[0].width,
        "the grown island is covered"
    );
}

/// Colour is drawn, so a state rule that recolours a module without changing a letter
/// of it still has to be reported.
#[test]
fn a_recoloured_module_is_damaged_even_with_the_same_words() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0

[module.cpu.states.hot]
above = 50
background = "#ff0000"
"##;
    let cool = frame_of(config, &[with_percent(item("cpu", "10%"), 10.0)]);
    let hot = frame_of(config, &[with_percent(item("cpu", "90%"), 90.0)]);
    let Damage::Rects(rects) = hot.damage(&cool) else {
        panic!("one module is not the whole bar");
    };
    assert!(!rects.is_empty(), "a colour change is a change");
}

/// The mode indicator is on the bar exactly while a mode is held. `default` is what a
/// keyboard does anyway, so it is drawn as nothing at all rather than as the word.
#[test]
fn a_binding_mode_is_shown_only_while_one_is_held() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["mode"]

[module.mode]
source = "sway:mode"
padding = 0
"##;
    let cfg = Config::parse(config).expect("test config parses");
    let mut sway = SwayState::default();
    let render = |sway: &SwayState| {
        let inputs = Inputs {
            items: &[],
            native: &Registry::new(&Default::default()),
            sway,
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed_groups: &Default::default(),
            switching: &Default::default(),
            folding: &Default::default(),
            module_folding: &Default::default(),
            collapsed: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: None,
        };
        let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
        frame.groups.first().map(|g| g.modules[0].text.clone())
    };

    // Before the compositor has answered, and while it is in the default mode, the
    // module draws nothing and takes the group with it.
    assert_eq!(render(&sway), None);
    sway.mode = Some(crate::sway::DEFAULT_MODE.to_string());
    assert_eq!(render(&sway), None);

    sway.mode = Some("resize".to_string());
    assert_eq!(render(&sway).as_deref(), Some(" resize "));

    // And it goes away again when the mode is left.
    sway.mode = Some(crate::sway::DEFAULT_MODE.to_string());
    assert_eq!(render(&sway), None);
}

#[test]
fn a_native_module_draws_what_its_collector_measured() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
padding = 0
format = "$utilization.n(d:0)"
"##;
    let mut fields = Fields::default();
    fields.set(
        "utilization",
        Value::Num {
            v: 42.0,
            unit: crate::status::Unit::Percent,
        },
    );
    fields.set_primary("utilization");
    let native = Registry::fixture(
        Which::Cpu,
        Reading {
            fields,
            state: crate::status::State::Idle,
        },
    );
    let frame = frame_with(config, &[], native);
    assert_eq!(frame.groups[0].modules[0].text, "42%");
}

/// One command reporting on three cities is three readings, and the module shows the
/// one it is scrolled to. The wording is the module's either way: a page is a reading
/// like any other, and nothing here knows it came from the same fetch as its
/// neighbours.
#[test]
fn a_source_that_said_several_things_shows_the_page_it_is_scrolled_to() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["weather"]

[module.weather]
source = "command"
command = ["weather"]
interval = "once"
pages = true
padding = 0
format = "$text"
"##;
    let said = |text: &str| {
        let mut fields = Fields::default();
        fields.set("text", Value::Text(text.to_string()));
        fields.set_primary("text");
        Reading {
            fields,
            state: crate::status::State::Idle,
        }
    };
    let which = Which::Command(crate::collect::CommandSpec {
        argv: vec!["weather".to_string()],
        run: crate::collect::command::Run::Once,
        pages: true,
        fields: crate::collect::command::PLAIN,
        timeout: std::time::Duration::from_secs(30),
    });
    let page = |showing: usize| {
        let native = Registry::fixture_pages(
            which.clone(),
            vec![said("Novi Sad"), said("Beograd"), said("Sokolac")],
        );
        let pages = std::collections::HashMap::from([("weather".to_string(), showing)]);
        frame_paged(
            config,
            &[],
            native,
            &Default::default(),
            &Default::default(),
            &pages,
        )
        .groups[0]
            .modules[0]
            .text
            .clone()
    };
    assert_eq!(page(0), "Novi Sad");
    assert_eq!(page(1), "Beograd");
    // A fetch that came back with fewer places than the last one leaves the module
    // pointing past the end, and it wraps rather than showing nothing.
    assert_eq!(page(4), "Beograd");
}

#[test]
fn a_command_with_a_run_out_shows_a_spinner_where_its_icon_goes() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["weather"]

[module.weather]
source = "command"
command = ["weather"]
interval = "30m"
icon = "$clock"
"##;
    let which = Which::Command(crate::collect::CommandSpec {
        argv: vec!["weather".to_string()],
        run: crate::collect::command::Run::Every(std::time::Duration::from_secs(1800)),
        pages: false,
        fields: crate::collect::command::PLAIN,
        timeout: std::time::Duration::from_secs(30),
    });
    let mut fields = Fields::default();
    fields.set("text", Value::Text("18C".to_string()));
    fields.set_primary("text");
    let reading = Reading {
        fields,
        state: crate::status::State::Idle,
    };
    let drawn = |waiting: bool| {
        let waiting = match waiting {
            true => std::collections::HashSet::from([which.clone()]),
            false => std::collections::HashSet::new(),
        };
        let module = frame_waiting(
            config,
            &[],
            Registry::fixture(which.clone(), reading.clone()),
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &waiting,
        )
        .groups[0]
            .modules[0]
            .clone();
        (module.text.clone(), module.icon.map(|i| (i.icon, i.level)))
    };
    // Nothing is out, so the module is its own icon and its last reading.
    assert_eq!(drawn(false), (" 18C ".to_string(), Some((Icon::Clock, 0))));
    // A run is out. The reading stays put - it is still the truth until the next one
    // lands - and the icon says that another is on its way.
    assert_eq!(drawn(true), (" 18C ".to_string(), Some((Icon::Spinner, 3))));
}

#[test]
fn a_command_waiting_on_its_first_answer_is_drawn_anyway() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["weather"]

[module.weather]
source = "command"
command = ["weather"]
interval = "once"
"##;
    let which = Which::Command(crate::collect::CommandSpec {
        argv: vec!["weather".to_string()],
        run: crate::collect::command::Run::Once,
        pages: false,
        fields: crate::collect::command::PLAIN,
        timeout: std::time::Duration::from_secs(30),
    });
    let frame = |waiting: std::collections::HashSet<Which>| {
        frame_waiting(
            config,
            &[],
            Registry::fixture(which.clone(), Reading::default()),
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &waiting,
        )
    };
    // Nothing said yet and nothing on its way: the module is not there at all, the
    // same as any other source that has not read.
    assert!(frame(Default::default()).groups.is_empty());
    // The first run is out. There is no wording to show and no icon in the config,
    // and the spinner is enough to carry the module on its own.
    let module =
        frame(std::collections::HashSet::from([which.clone()])).groups[0].modules[0].clone();
    assert_eq!(module.text, "");
    assert_eq!(module.icon.map(|i| i.icon), Some(Icon::Spinner));
}

#[test]
fn a_native_module_that_has_not_read_yet_draws_nothing() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
"##;
    let native = Registry::fixture(Which::Cpu, Reading::default());
    // No fields yet, so the format renders empty and the module is not there.
    assert!(frame_with(config, &[], native).groups.is_empty());
}

#[test]
fn modules_select_items_by_name() {
    let frame = frame_of(BASIC, &[item("mem", "50%"), item("cpu", "10%")]);
    let texts: Vec<&str> = frame.groups[0]
        .modules
        .iter()
        .map(|m| m.text.as_str())
        .collect();
    // Group order wins over the order the source sent them in.
    assert_eq!(texts, ["10%", "50%"]);
}

#[test]
fn an_unmatched_item_draws_nothing() {
    let frame = frame_of(BASIC, &[item("disk", "1G")]);
    assert!(frame.groups.is_empty());
}

#[test]
fn an_empty_item_is_hidden() {
    // The i3bar protocol uses empty text to mean "hide this block", and a native
    // source with nothing to say lands in the same place.
    let frame = frame_of(BASIC, &[item("cpu", ""), item("mem", "50%")]);
    assert_eq!(frame.groups[0].modules.len(), 1);
    assert_eq!(frame.groups[0].modules[0].text, "50%");
}

#[test]
fn a_wildcard_group_takes_every_item_in_order() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["*"]
"##;
    let frame = frame_of(config, &[item("mem", "50%"), item("cpu", "10%")]);
    let texts: Vec<&str> = frame.groups[0]
        .modules
        .iter()
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(texts, ["50%", "10%"]);
}

#[test]
fn source_colours_win_over_the_style() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
foreground = "#00ff00"
"##;
    let mut styled = item("cpu", "10%");
    styled.foreground = Some(Color::rgba(0xff, 0, 0, 0xff));
    let frame = frame_of(config, &[styled]);
    assert_eq!(
        frame.groups[0].modules[0].foreground,
        Color::rgba(0xff, 0, 0, 0xff)
    );
}

#[test]
fn a_module_says_what_its_format_says() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
format = "cpu $percent.n(w:3)"
"##;
    let frame = frame_of(config, &[with_percent(item("cpu", "ignored"), 7.0)]);
    // The width counts the whole number, suffix included, so a column of them lines up.
    assert_eq!(frame.groups[0].modules[0].text, "cpu  7%");
}

#[test]
fn a_format_can_drop_what_the_source_could_not_measure() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
format = "cpu{ $percent}"
"##;
    // No percentage in the text, so the source published none and the group goes.
    let frame = frame_of(config, &[item("cpu", "busy")]);
    assert_eq!(frame.groups[0].modules[0].text, "cpu");
}

const TWO_TONE: &str = r##"
[colors]
a = "#ff0000"
b = "#0000ff"

[left]
groups = ["g"]

[group.g]
modules = ["one", "two"]

[module.one]
padding = 0
background = "$a"

[module.two]
padding = 0
background = "$b"
"##;

#[test]
fn a_separator_is_never_the_same_colour_on_both_sides() {
    // Whichever neighbour the shape takes its colour from, the ground behind it has to
    // be the other one, or the boundary is invisible.
    for mode in ["previous", "next"] {
        let config = format!(
            "{TWO_TONE}\n[group.g.separator]\nshape = \"chevron\"\nwidth = 4\ncolor = \"{mode}\"\n"
        );
        let frame = frame_of(&config, &[item("one", "a"), item("two", "b")]);
        let sep = &frame.groups[0].separators[0];
        assert_ne!(sep.fill, sep.under, "with color = {mode:?}");
    }
}

#[test]
fn an_end_comes_to_a_point_over_whatever_is_behind_the_bar() {
    let config =
        format!("{TWO_TONE}\n[group.g.ends]\nleft = \"chevron\"\nright = \"chevron\"\nwidth = 6\n");
    let frame = frame_of(&config, &[item("one", "a"), item("two", "b")]);
    let group = &frame.groups[0];
    assert_eq!(group.separators.len(), 2, "one end at each side");

    // Each end pairs its module's colour with nothing, so the shape reads against the
    // bar rather than against another module.
    for end in &group.separators {
        let colours = [end.fill, end.under];
        assert!(
            colours.contains(&Color::TRANSPARENT),
            "an end must leave one side clear: {colours:?}"
        );
    }

    // The ends are drawn beside the modules, not over them.
    let modules_width: f32 = group.modules.iter().map(|m| m.width).sum();
    assert_eq!(group.width, modules_width + 12.0);
    assert_eq!(group.modules[0].x, group.x + 6.0);
}

#[test]
fn a_group_without_ends_reserves_no_room_for_them() {
    let frame = frame_of(TWO_TONE, &[item("one", "a"), item("two", "b")]);
    let group = &frame.groups[0];
    assert!(group.separators.is_empty());
    assert_eq!(group.modules[0].x, group.x);
}

#[test]
fn a_click_target_marks_a_module_that_has_a_second_wording() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu", "mem"]

[module.cpu]
padding = 0
format_alt = "$percent"

[module.mem]
padding = 0
"##;
    let frame = frame_of(config, &[item("cpu", "a"), item("mem", "b")]);
    assert_eq!(
        frame.groups[0].modules[0].alt,
        Some(2),
        "one further wording is two views to go round"
    );
    assert_eq!(
        frame.groups[0].modules[0].name.as_deref(),
        Some("cpu"),
        "a click has to know which module it is turning"
    );
    assert_eq!(frame.groups[0].modules[1].alt, None);
    assert_eq!(
        frame.groups[0].modules[1].name, None,
        "a module no gesture names carries no name"
    );
}

#[test]
fn the_second_wording_is_what_is_drawn_once_it_is_showing() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
format = "short"
format_alt = "the long way round"
"##;
    let showing = std::collections::HashMap::from([("cpu".to_string(), 1)]);
    let plain = frame_of(config, &[item("cpu", "x")]);
    assert_eq!(plain.groups[0].modules[0].text, "short");

    let swapped = frame_showing(
        config,
        &[item("cpu", "x")],
        Registry::new(&Default::default()),
        &showing,
    );
    assert_eq!(swapped.groups[0].modules[0].text, "the long way round");
}

#[test]
fn a_module_can_have_several_further_wordings_and_goes_round_them() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
format = "first"
format_alt = ["second", "third"]
"##;
    let views = |showing: usize| {
        frame_showing(
            config,
            &[item("cpu", "x")],
            Registry::new(&Default::default()),
            &std::collections::HashMap::from([("cpu".to_string(), showing)]),
        )
        .groups[0]
            .modules[0]
            .text
            .clone()
    };
    assert_eq!(views(0), "first");
    assert_eq!(views(1), "second");
    assert_eq!(views(2), "third");

    let frame = frame_of(config, &[item("cpu", "x")]);
    assert_eq!(
        frame.groups[0].modules[0].alt,
        Some(3),
        "three views, so a click wraps after the third"
    );
}

/// The width of a one-module bar built from these style keys, with the stub measurer
/// making every character one unit wide.
fn width_of(keys: &str) -> f32 {
    let config = format!(
        r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]
padding = 0
spacing = 0

[module.cpu]
{keys}
"##
    );
    let frame = frame_of(&config, &[item("cpu", "abc")]);
    frame.groups[0].modules[0].width
}

#[test]
fn padding_is_added_on_both_sides_and_nowhere_else() {
    // Three characters, no icon: the module is the text plus a padding each side.
    assert_eq!(width_of("padding = 0"), 3.0);
    assert_eq!(width_of("padding = 5"), 13.0);
    assert_eq!(width_of("padding = 5.5"), 14.0);
}

#[test]
fn the_gap_between_an_icon_and_its_text_is_the_configured_one() {
    let keys = |gap: &str| format!("padding = 0\nicon = \"$cpu\"\nicon_size = 10\n{gap}");
    // Icon, gap, then the text.
    assert_eq!(width_of(&keys("icon_gap = 0")), 13.0);
    assert_eq!(width_of(&keys("icon_gap = 4")), 17.0);
    // Without one, the gap is a quarter of the icon.
    assert_eq!(width_of(&keys("")), 15.5);
}

#[test]
fn a_bigger_icon_keeps_its_breathing_room_without_being_told() {
    let width = |size: f32| width_of(&format!("padding = 0\nicon = \"$cpu\"\nicon_size = {size}"));
    // Twice the icon is twice the gap, so the proportions hold as the bar grows.
    assert_eq!(width(10.0) - 3.0, 12.5);
    assert_eq!(width(20.0) - 3.0, 25.0);
}

#[test]
fn a_battery_is_given_the_room_a_long_icon_needs() {
    let width = |icon: &str| {
        width_of(&format!(
            "padding = 0\nicon_gap = 0\nicon_size = 20\nicon = \"${icon}\""
        ))
    };
    // Three characters of text either way, and a square icon takes its size.
    assert_eq!(width("cpu"), 23.0);
    // The battery asks for a quarter more, and the text starts behind all of it.
    assert_eq!(width("battery"), 28.0);
    assert_eq!(width("battery-charging"), 28.0);
}

#[test]
fn a_full_bar_truncates_rather_than_drawing_over_itself() {
    // The stub measurer makes every character a unit wide, and the bar is 200 of them.
    let config = r##"
[left]
groups = ["l"]

[center]
groups = ["c"]

[right]
groups = ["r"]

[group.l]
modules = ["title"]
padding = 0

[group.c]
modules = ["media"]
padding = 0

[group.r]
modules = ["clock"]
padding = 0

[module.title]
padding = 0

[module.media]
padding = 0

[module.clock]
padding = 0
"##;
    let long = "x".repeat(150);
    let frame = frame_of(
        config,
        &[
            item("title", &long),
            item("media", &long),
            item("clock", "12:00"),
        ],
    );

    let mut edges: Vec<(f32, f32)> = frame
        .groups
        .iter()
        .flat_map(|g| g.modules.iter())
        .map(|m| (m.x, m.x + m.width))
        .collect();
    edges.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("finite"));
    for pair in edges.windows(2) {
        assert!(
            pair[0].1 <= pair[1].0,
            "modules overlap: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
    let last = edges.last().expect("something was drawn");
    assert!(
        last.1 <= 200.0,
        "the bar draws past its own width: {last:?}"
    );
}

#[test]
fn a_folded_module_keeps_its_icon_and_loses_its_text() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
icon = "$cpu"
icon_size = 10
collapsible = true
"##;
    let open = frame_of(config, &[item("cpu", "a long wording")]);
    assert_eq!(open.groups[0].modules[0].text, "a long wording");
    assert!(
        open.groups[0].modules[0].collapsible,
        "the frame has to say a right click can fold it"
    );
    assert_eq!(open.groups[0].modules[0].name.as_deref(), Some("cpu"));

    let folded = frame_folded(
        config,
        &[item("cpu", "a long wording")],
        Registry::new(&Default::default()),
        &Default::default(),
        &std::collections::HashSet::from(["cpu".to_string()]),
    );
    assert_eq!(folded.groups[0].modules[0].text, "");
    assert!(
        folded.groups[0].modules[0].icon.is_some(),
        "the icon is what is left to click on"
    );
    assert!(
        folded.groups[0].modules[0].width < open.groups[0].modules[0].width,
        "folding is only worth doing if it takes less room"
    );
}

#[test]
fn a_folded_module_is_its_icon_centred_and_nothing_else() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 6
icon = "$cpu"
icon_size = 12
collapsible = true
"##;
    let folded = frame_folded(
        config,
        &[item("cpu", "50%")],
        Registry::new(&Default::default()),
        &Default::default(),
        &std::collections::HashSet::from(["cpu".to_string()]),
    );
    let module = &folded.groups[0].modules[0];
    // The icon and its padding, and not the gap that would have separated it from
    // text there is none of: 12 + 6 + 6.
    assert_eq!(module.width, 24.0);
    let icon = module.icon.as_ref().expect("the icon is what is left");
    // Which is what leaves the same room either side of it.
    assert_eq!(icon.x - module.x, 6.0);
    assert_eq!((module.x + module.width) - (icon.x + icon.size), 6.0);
}

#[test]
fn a_module_with_nothing_to_say_still_disappears_when_it_is_not_folded() {
    // An empty wording means "hide this", and that has to keep working next to a
    // module that is empty on purpose.
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
icon = "$cpu"
collapsible = true
"##;
    let frame = frame_of(config, &[item("cpu", "")]);
    assert!(frame.groups.is_empty() || frame.groups[0].modules.is_empty());
}

#[test]
fn a_rule_can_key_on_how_the_source_rates_itself() {
    let config = r##"
[colors]
bad = "#ff0000"

[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
format = "x"

[module.cpu.states.broken]
state = "error"
foreground = "$bad"
"##;
    let mut fields = Fields::default();
    fields.set(
        "utilization",
        Value::Num {
            v: 1.0,
            unit: crate::status::Unit::Percent,
        },
    );
    fields.set_primary("utilization");

    let working = Registry::fixture(
        Which::Cpu,
        Reading {
            fields: fields.clone(),
            state: State::Idle,
        },
    );
    assert_ne!(
        frame_with(config, &[], working).groups[0].modules[0].foreground,
        Color::rgba(0xff, 0, 0, 0xff)
    );

    let broken = Registry::fixture(
        Which::Cpu,
        Reading {
            fields,
            state: State::Error,
        },
    );
    assert_eq!(
        frame_with(config, &[], broken).groups[0].modules[0].foreground,
        Color::rgba(0xff, 0, 0, 0xff)
    );
}

#[test]
fn a_rule_can_match_a_word_the_source_published() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["bat"]

[module.bat]
source = "battery"
format = "x"
icon = "$battery"

[module.bat.states.charging]
field = "status"
equals = "charging"
icon = "$battery-charging"
"##;
    let charging = |status: &str| {
        let mut fields = Fields::default();
        fields.set(
            "percent",
            Value::Num {
                v: 50.0,
                unit: crate::status::Unit::Percent,
            },
        );
        fields.set("status", Value::Text(status.to_string()));
        fields.set_primary("percent");
        Registry::fixture(
            Which::Battery,
            Reading {
                fields,
                state: State::Idle,
            },
        )
    };

    let on = frame_with(config, &[], charging("charging"));
    assert_eq!(
        on.groups[0].modules[0].icon.as_ref().unwrap().icon,
        Icon::BatteryCharging
    );

    let off = frame_with(config, &[], charging("discharging"));
    assert_eq!(
        off.groups[0].modules[0].icon.as_ref().unwrap().icon,
        Icon::Battery
    );
}

#[test]
fn a_threshold_can_read_a_field_other_than_the_main_one() {
    let config = r##"
[colors]
warn = "#ffff00"

[left]
groups = ["g"]

[group.g]
modules = ["mem"]

[module.mem]
source = "memory"
format = "x"

[module.mem.states.swapping]
field = "swap_percent"
above = 20
foreground = "$warn"
"##;
    let unit = crate::status::Unit::Percent;
    let mut fields = Fields::default();
    // The main value is calm; the one the rule names is not.
    fields.set("percent", Value::Num { v: 5.0, unit });
    fields.set("swap_percent", Value::Num { v: 90.0, unit });
    fields.set_primary("percent");

    let native = Registry::fixture(
        Which::Memory,
        Reading {
            fields,
            state: State::Idle,
        },
    );
    assert_eq!(
        frame_with(config, &[], native).groups[0].modules[0].foreground,
        Color::rgba(0xff, 0xff, 0, 0xff)
    );
}

#[test]
fn thresholds_key_on_the_published_value_not_the_text() {
    let config = r##"
[colors]
warn = "#ffff00"

[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu.states.high]
above = 80
foreground = "$warn"
"##;
    // The text says nothing a threshold could be scraped from; the field carries it.
    let hot = with_percent(item("cpu", "busy"), 90.0);
    let frame = frame_of(config, &[hot]);
    assert_eq!(
        frame.groups[0].modules[0].foreground,
        Color::rgba(0xff, 0xff, 0, 0xff)
    );

    let cool = with_percent(item("cpu", "busy"), 10.0);
    let frame = frame_of(config, &[cool]);
    assert_ne!(
        frame.groups[0].modules[0].foreground,
        Color::rgba(0xff, 0xff, 0, 0xff)
    );
}

#[test]
fn a_graded_icon_takes_its_level_from_the_published_value() {
    let config = r##"
[bar]
icon_size = 10

[left]
groups = ["g"]

[group.g]
modules = ["bat"]

[module.bat]
icon = "$battery"
"##;
    for (percent, level) in [(0.0, 0), (50.0, 2), (100.0, 4)] {
        let frame = frame_of(config, &[with_percent(item("bat", "x"), percent)]);
        assert_eq!(
            frame.groups[0].modules[0].icon.as_ref().unwrap().level,
            level,
            "at {percent}%"
        );
    }
}

#[test]
fn clicks_route_to_whatever_the_source_asked_for() {
    let mut clickable = item("cpu", "10%");
    clickable.action = Some(ActionTarget::I3Bar {
        name: Some("0".to_string()),
        instance: None,
    });
    let frame = frame_of(BASIC, &[clickable]);
    let action = frame.groups[0].modules[0].action.as_ref();
    assert!(matches!(
        action,
        Some(ActionTarget::I3Bar { name: Some(n), .. }) if n == "0"
    ));
}

#[test]
fn a_module_wider_than_max_width_loses_text_not_its_neighbours() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu", "mem"]

[module.cpu]
padding = 0
max_width = 5

[module.mem]
padding = 0
"##;
    let frame = frame_of(config, &[item("cpu", "0123456789"), item("mem", "ab")]);
    let modules = &frame.groups[0].modules;
    assert_eq!(modules[0].width, 5.0);
    assert_eq!(modules[0].text, "0123\u{2026}");
    assert_eq!(modules[1].x, 5.0);
}

#[test]
fn hit_testing_finds_the_module_under_a_point() {
    let frame = frame_of(BASIC, &[item("cpu", "abc"), item("mem", "de")]);
    assert_eq!(frame.module_at(1.0, 5.0).unwrap().text, "abc");
    assert_eq!(frame.module_at(4.0, 5.0).unwrap().text, "de");
    assert!(frame.module_at(50.0, 5.0).is_none());
}
const JOINED: &str = r##"
[bar]
height = 20
gap = 17
[right]
groups = ["a", "b", "c"]
[right.separator]
shape = "slant"
width = 5
overlap = 1
[group.a]
modules = ["a"]
radius = 4
ends = { left = "slant", right = "slant", width = 2 }
[group.b]
modules = ["b"]
radius = 4
ends = { left = "slant", right = "slant", width = 2 }
[group.c]
modules = ["c"]
radius = 4
ends = { left = "slant", right = "slant", width = 2 }
[module.a]
format = "$text"
padding = 0
background = "#aa0000"
[module.b]
format = "$text"
padding = 0
background = "#00aa00"
[module.b.states.hover]
hover = true
background = "#ffff00"
[module.c]
format = "$text"
padding = 0
background = "#0000aa"
"##;

fn joined_frame(
    config: &str,
    items: &[StatusItem],
    width: f32,
    pointer: Option<(f32, f32)>,
) -> Frame {
    let cfg = Config::parse(config).unwrap();
    let inputs = Inputs {
        items,
        native: &Registry::new(&Default::default()),
        sway: &SwayState::default(),
        alt: &Default::default(),
        pages: &Default::default(),
        collapsed_groups: &Default::default(),
        switching: &Default::default(),
        folding: &Default::default(),
        module_folding: &Default::default(),
        collapsed: &Default::default(),
        waiting: &Default::default(),
        spin: 0,
        tray: &Default::default(),
        output: None,
    };
    compute(&cfg, &inputs, width, 20.0, &mut Fixed, pointer)
}

#[test]
fn joined_groups_skip_empty_neighbors_and_keep_only_outer_caps() {
    let items = [item("a", "aaa"), item("b", ""), item("c", "ccc")];
    let frame = joined_frame(JOINED, &items, 100.0, None);
    assert_eq!(frame.groups.len(), 2);
    assert_eq!(frame.group_separators.len(), 1);
    let (a, c) = (&frame.groups[0], &frame.groups[1]);
    assert_eq!((a.width, c.width), (5.0, 5.0)); // three letters, one outer cap
    assert_eq!(c.x - (a.x + a.width), 5.0); // join, not bar.gap or facing caps
    assert_eq!(
        (a.edges.left, a.edges.right),
        (EdgeShape::Round, EdgeShape::None)
    );
    assert_eq!(
        (c.edges.left, c.edges.right),
        (EdgeShape::None, EdgeShape::Round)
    );
    assert_eq!((a.separators.len(), c.separators.len()), (1, 1));
    let join = &frame.group_separators[0];
    assert_eq!(join.fill, a.modules[0].background);
    assert_eq!(join.under, c.modules[0].background);
    assert!(frame.module_at(join.x + 2.0, 10.0).is_none());
    let single = joined_frame(JOINED, &[item("b", "bbb")], 100.0, None);
    assert!(single.group_separators.is_empty());
    assert_eq!(single.groups[0].width, 7.0);
    assert_eq!(single.groups[0].separators.len(), 2);
    let empty = joined_frame(JOINED, &[], 100.0, None);
    assert!(empty.groups.is_empty() && empty.group_separators.is_empty());
}

/// A group that is not joined to anything has the same arithmetic to do: the room
/// between two modules is the separator's when there is one, and the ends are drawn
/// beside the modules rather than over them. Sizing against `spacing` while charging
/// the separator's width let a group compute itself wider than the bar it was being
/// fitted into, and then draw there.
#[test]
fn a_group_fits_its_separators_and_ends_in_the_width_it_was_given() {
    let config = r##"
[bar]
height = 20
gap = 0
[right]
groups = ["a"]
[group.a]
modules = ["a", "b", "c"]
spacing = 0
separator = { shape = "slant", width = 20 }
ends = { left = "slant", right = "slant", width = 6 }
[module.a]
format = "$text"
padding = 0
[module.b]
format = "$text"
padding = 0
[module.c]
format = "$text"
padding = 0
"##;
    let items = [item("a", "aaaa"), item("b", "bbbb"), item("c", "cccc")];
    for width in 1..120 {
        let width = width as f32;
        let frame = joined_frame(config, &items, width, None);
        for group in &frame.groups {
            assert!(
                group.x >= 0.0 && group.x + group.width <= width,
                "width {width}: a group {} wide sits at {}",
                group.width,
                group.x
            );
        }
    }

    // And what it does fit is what it says it is: three modules of four characters,
    // two twenty-wide separators between them, and six at each end.
    let frame = joined_frame(config, &items, 120.0, None);
    assert_eq!(frame.groups[0].width, 4.0 * 3.0 + 20.0 * 2.0 + 6.0 * 2.0);
}

#[test]
fn joined_groups_fit_caps_and_internal_separators_in_the_width_budget() {
    let config = JOINED.replace(
        "modules = [\"a\"]",
        "modules = [\"a\", \"b\"]\nseparator = { shape = 'slant', width = 9 }",
    );
    let items = [
        item("a", "aaaaaaaaaa"),
        item("b", "bbbbbbbbbb"),
        item("c", "cccccccccc"),
    ];
    for width in 1..90 {
        let frame = joined_frame(&config, &items, width as f32, None);
        for group in &frame.groups {
            assert!(
                group.x >= 0.0 && group.x + group.width <= width as f32,
                "width {width}: {group:?}"
            );
            for m in &group.modules {
                assert!(m.x >= group.x && m.x + m.width <= group.x + group.width);
            }
        }
        assert_eq!(
            frame.group_separators.len(),
            frame.groups.len().saturating_sub(1)
        );
    }
}

#[test]
fn joins_use_hover_and_source_colors_and_damage_both_old_and_new_bounds() {
    let mut items = [item("a", "aaa"), item("b", "bbb"), item("c", "ccc")];
    let old = joined_frame(JOINED, &items, 100.0, None);
    let b = &old.groups[1].modules[0];
    let hovered = joined_frame(JOINED, &items, 100.0, Some((b.x + 1.0, 10.0)));
    let yellow = Color::parse("#ffff00").unwrap();
    assert_eq!(hovered.group_separators[0].under, yellow);
    assert_eq!(hovered.group_separators[1].fill, yellow);
    let Damage::Rects(rects) = hovered.damage(&old) else {
        panic!("hover keeps geometry");
    };
    for join in &hovered.group_separators {
        assert!(rects.contains(&(join.x - 1.0, 0.0, 7.0, 20.0)));
    }
    items[1].background = Some(Color::parse("#ff7700").unwrap());
    let changed = joined_frame(JOINED, &items, 100.0, None);
    assert_eq!(
        changed.group_separators[0].under,
        items[1].background.unwrap()
    );
    assert_eq!(
        changed.group_separators[1].fill,
        items[1].background.unwrap()
    );
    assert!(matches!(changed.damage(&changed),Damage::Rects(r) if r.is_empty()));
    let resized = joined_frame(JOINED, &items, 110.0, None);
    let Damage::Rects(rects) = resized.damage(&changed) else {
        panic!("same groups");
    };
    for join in changed
        .group_separators
        .iter()
        .chain(&resized.group_separators)
    {
        assert!(rects.contains(&(join.x - 1.0, 0.0, 7.0, 20.0)));
    }
    assert!(matches!(
        joined_frame(JOINED, &items[..1], 100.0, None).damage(&old),
        Damage::All
    ));
}

/// An end cap bleeds `overlap` past itself, and it sits at the very edge of the island,
/// so a group with less padding than overlap paints outside its own rectangle. Square
/// edges have no clip mask to catch that, so the pixels are really there and damage has
/// to name them - otherwise a recoloured group leaves the old cap's edge on screen.
#[test]
fn end_caps_are_inside_the_damage_a_recoloured_group_reports() {
    const CAPPED: &str = r##"
[bar]
height = 20
[right]
groups = ["a"]
[group.a]
modules = ["a"]
padding = 0
ends = { left = "slant", right = "slant", width = 2, overlap = 3 }
[module.a]
format = "$text"
padding = 0
background = "#aa0000"
"##;
    let mut items = [item("a", "aaa")];
    let before = joined_frame(CAPPED, &items, 100.0, None);
    let group = &before.groups[0];
    let (bx, _, bw, _) = group.paint_bounds();
    assert_eq!(bx, group.x - 3.0);
    assert_eq!(bw, group.width + 6.0);

    items[0].background = Some(Color::parse("#00aa00").unwrap());
    let after = joined_frame(CAPPED, &items, 100.0, None);
    let Damage::Rects(rects) = after.damage(&before) else {
        panic!("only the colour changed");
    };
    assert!(!rects.is_empty());
    for (x, _, w, _) in &rects {
        assert!(
            *x <= group.x - 3.0 && x + w >= group.x + group.width + 3.0,
            "damage {x}+{w} does not cover the caps of a group at {}+{}",
            group.x,
            group.width
        );
    }
}

#[test]
fn absent_or_disabled_group_separator_preserves_independent_layout() {
    let disabled = JOINED.replace(
        "shape = \"slant\"\nwidth = 5",
        "shape = \"none\"\nwidth = 5",
    );
    let omitted = disabled.replace(
        "[right.separator]\nshape = \"none\"\nwidth = 5\noverlap = 1\n",
        "",
    );
    let items = [item("a", "aaa"), item("b", "bbb"), item("c", "ccc")];
    let a = joined_frame(&disabled, &items, 100.0, None);
    let b = joined_frame(&omitted, &items, 100.0, None);
    assert!(a.group_separators.is_empty());
    assert!(matches!(a.damage(&b),Damage::Rects(r) if r.is_empty()));
    assert_eq!(a.groups[1].x - a.groups[0].x - a.groups[0].width, 17.0);
}
#[test]
fn outer_caps_can_face_left_without_reversing_group_joins() {
    let config = JOINED.replacen(
        "ends = { left = \"slant\", right = \"slant\", width = 2 }",
        "ends = { left = \"slant\", right = \"slant\", width = 2, direction = \"left\" }",
        1,
    );
    let items = [item("a", "aaa"), item("b", "bbb")];
    let frame = joined_frame(&config, &items, 100.0, None);
    let leading = &frame.groups[0].separators[0];
    assert_eq!(leading.direction, Direction::Left);
    assert_eq!(leading.fill, Color::TRANSPARENT);
    assert_eq!(leading.under, frame.groups[0].modules[0].background);
    assert_eq!(frame.group_separators[0].direction, Direction::Right);
    // The next group's outer cap still inherits its own separator direction.
    assert_eq!(frame.groups[1].separators[0].direction, Direction::Right);
    let original = joined_frame(JOINED, &items, 100.0, None);
    assert_eq!(original.groups[0].separators[0].direction, Direction::Right);
}
thread_local! {
    pub(super) static COLLECTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn collapse_config() -> String {
    let mut config = JOINED.to_string();
    for name in ["a", "b", "c"] {
        config = config.replace(&format!("[group.{name}]"), &format!(
                "[group.{name}]\ncollapsible = true\ncollapse_button = 'right'\ncollapsed = {{ icon = '$cpu', icon_size = 8, padding = 2, background = '#83a598' }}"
            ));
    }
    config
}

fn group_inputs<'a>(
    items: &'a [StatusItem],
    native: &'a Registry,
    groups: &'a std::collections::HashSet<String>,
) -> Inputs<'a> {
    // Empty shared fixtures live for the duration of each test thread.
    static EMPTY_SET: std::sync::LazyLock<std::collections::HashSet<String>> =
        std::sync::LazyLock::new(Default::default);
    static EMPTY_MAP: std::sync::LazyLock<std::collections::HashMap<String, usize>> =
        std::sync::LazyLock::new(Default::default);
    static SWAY: std::sync::LazyLock<SwayState> = std::sync::LazyLock::new(Default::default);
    static TRAY: std::sync::LazyLock<crate::tray::TrayState> =
        std::sync::LazyLock::new(Default::default);
    static WAITING: std::sync::LazyLock<std::collections::HashSet<Which>> =
        std::sync::LazyLock::new(Default::default);
    static SETTLED: std::sync::LazyLock<std::collections::HashMap<String, f32>> =
        std::sync::LazyLock::new(Default::default);
    static SHOWING: std::sync::LazyLock<std::collections::HashMap<String, Leaving>> =
        std::sync::LazyLock::new(Default::default);
    Inputs {
        items,
        native,
        sway: &SWAY,
        alt: &EMPTY_MAP,
        pages: &EMPTY_MAP,
        collapsed: &EMPTY_SET,
        collapsed_groups: groups,
        switching: &SHOWING,
        folding: &SETTLED,
        module_folding: &SETTLED,
        waiting: &WAITING,
        spin: 0,
        tray: &TRAY,
        output: None,
    }
}

/// A fold hands over to the collapsed style at the end of its travel: the island is
/// left holding that style's icon on its ground, so the module that stays behind has
/// to arrive wearing them or the last frame is a colour change nobody asked for.
#[test]
fn a_fold_carries_its_first_module_over_to_the_collapsed_colours() {
    let config = r##"
[bar]
height = 34
icon_size = 17
[right]
groups = ["s"]
[group.s]
modules = ["cpu", "mem"]
collapsible = true
collapse_button = "right"
padding = 2
collapsed = { icon = "$cpu", padding = 6, background = "#83a598", foreground = "#282828" }
[module.cpu]
format = "$text"
padding = 6
icon = "$cpu"
background = "#cc241d"
foreground = "#ebdbb2"
[module.cpu.states.hover]
hover = true
background = "#fb4934"
[module.mem]
format = "$text"
padding = 6
icon = "$memory"
background = "#458588"
"##;
    let cfg = Config::parse(config).unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("cpu", "42"), item("mem", "70")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let all = ["s".to_string()].into_iter().collect();
    inputs.collapsed_groups = &all;
    let shut = compute(&cfg, &inputs, 400.0, 34.0, &mut Fixed, None);
    inputs.collapsed_groups = &none;
    let shut = &shut.groups[0].modules[0];

    let ends: [std::collections::HashMap<String, f32>; 3] = [
        [("s".to_string(), 0.0)].into(),
        [("s".to_string(), 0.5)].into(),
        [("s".to_string(), 1.0)].into(),
    ];
    let mut colours = Vec::new();
    for at in &ends {
        inputs.folding = at;
        let frame = compute(&cfg, &inputs, 400.0, 34.0, &mut Fixed, None);
        let first = &frame.groups[0].modules[0];
        colours.push((first.foreground, first.background));
    }

    // Leaves as itself, arrives as the shut island, and is neither in between.
    assert_eq!(
        colours[0],
        (
            Color::parse("#ebdbb2").unwrap(),
            Color::parse("#cc241d").unwrap()
        )
    );
    assert_eq!(colours[2], (shut.foreground, shut.background));
    assert!(colours[1] != colours[0] && colours[1] != colours[2]);

    // A pointer resting on the island it just folded would otherwise hold the module
    // at its hover colours all the way and land on the collapsed ones in one step.
    let at: std::collections::HashMap<String, f32> = [("s".to_string(), 1.0)].into();
    inputs.folding = &at;
    let over = compute(&cfg, &inputs, 400.0, 34.0, &mut Fixed, Some((398.0, 17.0)));
    let hovered = &over.groups[0].modules[0];
    assert_eq!(
        (hovered.foreground, hovered.background),
        (shut.foreground, shut.background)
    );
}

/// A fold gives its released width back to the bar as it travels, so its neighbours
/// move. What they are measured against must not move with them: a group charged what
/// it is showing hands everything after it a larger budget on every frame, and a title
/// downstream sheds and regains a character at a time all the way through the fold.
#[test]
fn a_fold_does_not_re_truncate_the_groups_it_makes_room_for() {
    let config = r##"
[bar]
height = 20
gap = 0
[right]
groups = ["s"]
[left]
groups = ["t"]
[group.s]
modules = ["cpu"]
collapsible = true
collapse_button = "right"
padding = 0
collapsed = { icon = "$cpu", icon_size = 8, padding = 0 }
[group.t]
modules = ["title"]
padding = 0
[module.cpu]
format = "$text"
padding = 0
[module.title]
format = "$text"
padding = 0
"##;
    let cfg = Config::parse(config).unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("cpu", &"c".repeat(40)), item("title", &"t".repeat(80))];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let title = |frame: &Frame| {
        let group = frame
            .groups
            .iter()
            .find(|g| g.x < 10.0)
            .expect("the left run");
        group.modules[0].text.clone()
    };

    let open = compute(&cfg, &inputs, 60.0, 20.0, &mut Fixed, None);
    assert!(
        title(&open).contains(ELLIPSIS),
        "the fixture has to be tight enough to cut"
    );

    // Every step of the travel says the same thing, and what it says is what the frame
    // the fold left from said.
    let steps: Vec<std::collections::HashMap<String, f32>> = (0..=10)
        .map(|step| [("s".to_string(), step as f32 / 10.0)].into())
        .collect();
    let mut widths = Vec::new();
    for (step, at) in steps.iter().enumerate() {
        inputs.folding = at;
        let frame = compute(&cfg, &inputs, 60.0, 20.0, &mut Fixed, None);
        assert_eq!(title(&frame), title(&open), "the title moved at {step}/10");
        widths.push(frame.groups.iter().find(|g| g.x >= 10.0).unwrap().width);
    }
    // And the island really did travel, so this is not a test of a fold that stood still.
    assert!(widths[0] > *widths.last().unwrap());

    // Settled shut, the room is genuinely handed over and the title takes it.
    let all = ["s".to_string()].into_iter().collect();
    let settled: std::collections::HashMap<String, f32> = Default::default();
    inputs.folding = &settled;
    inputs.collapsed_groups = &all;
    let shut = compute(&cfg, &inputs, 60.0, 20.0, &mut Fixed, None);
    assert!(title(&shut).len() > title(&open).len());
}

/// The island a fold arrives at is one icon, and the icon the first module already
/// draws is the one it lands on: a config names the same picture for both so the fold
/// reads as everything else sliding away from it. That only works if the two agree on
/// where the icon is, so the contents travel with the fold rather than sitting still.
#[test]
fn a_fold_lands_its_icon_where_the_shut_island_draws_one() {
    let config = r##"
[bar]
height = 34
icon_size = 17
[right]
groups = ["s"]
[group.s]
modules = ["cpu", "mem"]
collapsible = true
collapse_button = "right"
padding = 2
spacing = 2
collapsed = { icon = "$cpu", padding = 6 }
[group.s.edges]
left = "round"
right = "round"
[module.cpu]
format = "$text"
min_width = 66
padding = 6
icon_gap = 3
icon = "$cpu"
[module.mem]
format = "$text"
padding = 6
icon = "$memory"
"##;
    let cfg = Config::parse(config).unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("cpu", "42"), item("mem", "70")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let icon_of = |frame: &Frame| {
        let group = &frame.groups[0];
        let icon = group.modules[0].icon.as_ref().expect("an icon to follow");
        (group.x, group.width, icon.x)
    };

    let open = compute(&cfg, &inputs, 400.0, 34.0, &mut Fixed, None);
    let all = ["s".to_string()].into_iter().collect();
    inputs.collapsed_groups = &all;
    let shut = compute(&cfg, &inputs, 400.0, 34.0, &mut Fixed, None);
    inputs.collapsed_groups = &none;

    // Nothing has moved on the frame a fold starts from, and the frame it ends on is
    // the shut island itself - icon included, which is the whole point of the travel.
    let ends: [std::collections::HashMap<String, f32>; 2] = [
        [("s".to_string(), 0.0)].into(),
        [("s".to_string(), 1.0)].into(),
    ];
    for (at, want) in ends.iter().zip([icon_of(&open), icon_of(&shut)]) {
        inputs.folding = at;
        let frame = compute(&cfg, &inputs, 400.0, 34.0, &mut Fixed, None);
        let got = icon_of(&frame);
        assert!(
            (got.0 - want.0).abs() < 0.001
                && (got.1 - want.1).abs() < 0.001
                && (got.2 - want.2).abs() < 0.001,
            "at {at:?}: {got:?} against {want:?}"
        );
    }

    // Halfway is halfway, and the content that ran past the island still has to be in
    // the bounds the mask is built from even though it has moved back over the edge.
    let halfway: std::collections::HashMap<String, f32> = [("s".to_string(), 0.5)].into();
    inputs.folding = &halfway;
    let frame = compute(&cfg, &inputs, 400.0, 34.0, &mut Fixed, None);
    let (x, _, icon) = icon_of(&frame);
    let travel = icon_of(&shut).2 - icon_of(&open).2 - (icon_of(&shut).0 - icon_of(&open).0);
    assert!((icon - (x + icon_of(&open).2 - icon_of(&open).0 + travel / 2.0)).abs() < 0.001);
    let group = &frame.groups[0];
    let (bx, _, bw, _) = group.paint_bounds();
    let first = &group.modules[0];
    let last = group.modules.last().unwrap();
    assert!(bx <= first.x && bx + bw >= last.x + last.width);
}

#[test]
fn a_folding_group_holds_its_open_content_at_a_travelling_width() {
    let cfg = Config::parse(&collapse_config()).unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("a", &"a".repeat(40)), item("b", "bb"), item("c", "cc")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let open = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    let all = ["a", "b", "c"].map(str::to_string).into();
    inputs.collapsed_groups = &all;
    let shut = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    inputs.collapsed_groups = &none;

    let (open_w, shut_w) = (open.groups[0].width, shut.groups[0].width);
    assert!(shut_w < open_w, "a shut group is the narrower of the two");
    let halfway: std::collections::HashMap<String, f32> = [("a".to_string(), 0.5)].into();
    inputs.folding = &halfway;
    let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    let travelling = &frame.groups[0];

    // Between the two shapes and cut off at its own edge, holding the open group's
    // children rather than a re-measured version of them.
    assert!((travelling.width - (open_w + shut_w) / 2.0).abs() < 0.001);
    assert!(travelling.content_right.is_some());
    assert_eq!(travelling.modules.len(), open.groups[0].modules.len());
    assert_eq!(travelling.modules[0].text, open.groups[0].modules[0].text);

    // The content reaches past the island, and the bounds the mask and the layer are
    // built from have to cover it or a fold would draw through a stale mask.
    let last = travelling.modules.last().unwrap();
    assert!(last.x + last.width > travelling.x + travelling.width);
    let (bx, _, bw, _) = travelling.paint_bounds();
    assert!(bx + bw >= last.x + last.width);

    // Both ends of the travel are the frames either side of it, so nothing jumps as
    // the fold starts or arrives.
    let start: std::collections::HashMap<String, f32> = [("a".to_string(), 0.0)].into();
    let end: std::collections::HashMap<String, f32> = [("a".to_string(), 1.0)].into();
    for (at, want) in [(&start, open_w), (&end, shut_w)] {
        inputs.folding = at;
        let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
        assert!((frame.groups[0].width - want).abs() < 0.001);
    }

    // Its neighbours pack against the width it is showing, not the one it is holding:
    // in a right-hand run that is the fold giving its released width back to the bar.
    inputs.folding = &halfway;
    let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    let a = &frame.groups[0];
    assert!(a.x > open.groups[0].x);
    let gap = |f: &Frame| f.groups[1].x - (f.groups[0].x + f.groups[0].width);
    assert!((gap(&frame) - gap(&open)).abs() < 0.001);
}

/// A join and a trailing cap belong to the island, not to the last module inside it.
/// A fold moves the island's edge in over its content, and anything left behind at the
/// content's edge is drawn outside the island - over whatever the fold made room for,
/// or clipped off the screen entirely.
#[test]
fn a_fold_carries_the_islands_join_and_cap_with_its_edge() {
    let cfg = Config::parse(&collapse_config()).unwrap();
    let native = Registry::new(&Default::default());
    let items = [
        item("a", &"a".repeat(40)),
        item("b", &"b".repeat(40)),
        item("c", &"c".repeat(40)),
    ];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let open = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);

    // The join between two islands keeps its place inside the edge it hangs from.
    let inset = |f: &Frame| (f.groups[0].x + f.groups[0].width) - f.group_separators[0].x;
    let folding: std::collections::HashMap<String, f32> = [("a".to_string(), 0.5)].into();
    inputs.folding = &folding;
    let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    assert!(frame.groups[0].content_right.is_some());
    assert!((inset(&frame) - inset(&open)).abs() < 0.001);
    let last = frame.groups[0].modules.last().unwrap();
    assert!(
        frame.group_separators[0].x < last.x + last.width,
        "the join stayed behind on the content"
    );
    // And the island beside it starts clear of that join rather than under it.
    assert!(frame.groups[1].x >= frame.group_separators[0].x + frame.group_separators[0].width);

    // The last island in a run keeps a trailing cap of its own, which travels too.
    let cap = |f: &Frame| {
        let group = f.groups.last().unwrap();
        (group.x + group.width) - group.separators.last().unwrap().x
    };
    let folding: std::collections::HashMap<String, f32> = [("c".to_string(), 0.5)].into();
    inputs.folding = &folding;
    let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    assert!(frame.groups[2].content_right.is_some());
    assert!((cap(&frame) - cap(&open)).abs() < 0.001);
    let last = frame.groups[2].modules.last().unwrap();
    assert!(
        frame.groups[2].separators.last().unwrap().x < last.x + last.width,
        "the cap stayed behind on the content"
    );
}

/// A fold does not only shrink. A group holding one narrow module can be closing over
/// an icon wider than all of it, and the widths it travels through are checked by
/// neither the open branch nor the shut one.
#[test]
fn a_fold_that_would_grow_out_of_its_run_is_left_out_of_it() {
    let cfg = Config::parse(
        "[bar]\ngap = 0\n[left]\ngroups = ['a']\n[group.a]\nmodules = ['a']\n\
             collapsible = true\ncollapse_button = 'right'\n\
             collapsed = { icon = '$cpu', icon_size = 40, padding = 6 }\n",
    )
    .unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("a", "x")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);

    // Wide enough for the one character it is showing, nowhere near the icon it is
    // folding down to.
    let open = compute(&cfg, &inputs, 20.0, 20.0, &mut Fixed, None);
    assert_eq!(open.groups.len(), 1);
    let shut = ["a".to_string()].into();
    inputs.collapsed_groups = &shut;
    assert!(
        compute(&cfg, &inputs, 20.0, 20.0, &mut Fixed, None)
            .groups
            .is_empty()
    );
    inputs.collapsed_groups = &none;

    let travel: Vec<std::collections::HashMap<String, f32>> = [0.0, 0.5, 0.9, 1.0]
        .into_iter()
        .map(|at| [("a".to_string(), at)].into())
        .collect();
    for folding in &travel[1..] {
        inputs.folding = folding;
        let frame = compute(&cfg, &inputs, 20.0, 20.0, &mut Fixed, None);
        assert!(frame.groups.is_empty());
    }
    // With room for both ends of it, the same fold is drawn all the way through.
    for folding in &travel {
        inputs.folding = folding;
        let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
        assert_eq!(frame.groups.len(), 1);
        assert!(frame.groups[0].x + frame.groups[0].width <= 400.0);
    }
}

#[test]
fn group_collapse_keeps_empty_children_and_never_measures_hidden_text() {
    struct NoMeasure;
    impl Measure for NoMeasure {
        fn measure(&mut self, _: &str) -> f32 {
            panic!("collapsed group measured text");
        }
    }
    let cfg = Config::parse(&collapse_config()).unwrap();
    let native = Registry::new(&Default::default());
    let groups = ["a", "b", "c"].map(str::to_string).into();
    let items = [
        item("a", &"hidden ".repeat(10_000)),
        item("b", ""),
        item("c", "hidden"),
    ];
    let mut inputs = group_inputs(&items, &native, &groups);
    COLLECTIONS.with(|count| count.set(0));
    let frame = compute(&cfg, &inputs, 200.0, 20.0, &mut NoMeasure, None);
    COLLECTIONS
        .with(|count| assert_eq!(count.get(), 0, "hidden children were collected/formatted"));
    assert_eq!(frame.groups.len(), 3);
    assert!(
        frame
            .groups
            .iter()
            .all(|g| g.modules.len() == 1 && g.modules[0].text.is_empty())
    );
    for group in &frame.groups {
        let icon = &group.modules[0];
        assert!(icon.action.is_none() && icon.on_click.is_none() && icon.name.is_none());
        assert!(
            icon.alt.is_none()
                && icon.refresh.is_none()
                && icon.mute.is_none()
                && icon.paged.is_none()
                && !icon.collapsible
        );
        assert!(matches!(
            frame.click_at(icon.x + 1.0, 10.0, 3),
            Some(ClickTarget::Group(_))
        ));
    }
    inputs.items = &[];
    let empty = compute(&cfg, &inputs, 200.0, 20.0, &mut NoMeasure, None);
    assert_eq!(empty.damage(&frame), Damage::Rects(vec![]));
    let expanded = Default::default();
    inputs.collapsed_groups = &expanded;
    assert!(
        compute(&cfg, &inputs, 200.0, 20.0, &mut NoMeasure, None)
            .groups
            .is_empty()
    );
}

#[test]
fn group_toggle_restores_child_views_pages_and_collapse_on_every_output() {
    let cfg = Config::parse(
        r#"
[left]
groups = ['g', 'other']
[group.g]
modules = ['folded', 'weather']
collapsible = true
collapse_button = 'right'
collapsed = { icon = '$cpu', icon_size = 8 }
[group.other]
modules = ['other']
collapsible = true
collapse_button = 'middle'
collapsed = { icon = '$memory', icon_size = 8 }
[module.folded]
icon = '$cpu'
collapsible = true
# Not the group's own button: a group answers for its whole island, so a child
# claiming the reserved one is a config error rather than a key that does nothing.
collapse_button = 'middle'
format_alt = 'alt $text'
[module.weather]
source = 'command'
command = ['weather']
interval = 'once'
pages = true
format = '$text'
format_alt = 'alt $text'
"#,
    )
    .unwrap();
    let Source::Native(which) = &cfg.positions[0].groups[0].modules[1].source else {
        panic!()
    };
    let reading = |value: &str| Reading {
        fields: item("", value).fields,
        state: Default::default(),
    };
    let mut native =
        Registry::fixture_pages(which.clone(), vec![reading("first"), reading("second")]);
    let items = [item("folded", "value"), item("other", "visible")];
    let mut groups = Default::default();
    let alt = [("weather".to_string(), 1), ("folded".to_string(), 1)].into();
    let pages = [("weather".to_string(), 1)].into();
    let child = ["folded".to_string()].into();
    let build = |native: &Registry, groups: &std::collections::HashSet<String>, output| {
        let mut inputs = group_inputs(&items, native, groups);
        inputs.alt = &alt;
        inputs.pages = &pages;
        inputs.collapsed = &child;
        inputs.output = output;
        compute(&cfg, &inputs, 400.0, 30.0, &mut Fixed, None)
    };
    let before = build(&native, &groups, Some("DP-1"));
    assert_eq!(before.groups[0].modules[0].text, "");
    assert_eq!(before.groups[0].modules[1].text, "alt second");
    groups.insert("g".to_string());
    let folded = build(&native, &groups, Some("DP-1"));
    let other_output = build(&native, &groups, Some("HDMI-A-1"));
    assert_eq!(folded.damage(&other_output), Damage::Rects(vec![]));
    assert_eq!(folded.groups[0].modules.len(), 1);
    assert_eq!(folded.groups[1].modules[0].text, "visible");
    native = Registry::fixture_pages(
        which.clone(),
        vec![reading("new first"), reading("new second")],
    );
    assert_eq!(
        build(&native, &groups, None).damage(&folded),
        Damage::Rects(vec![])
    );
    groups.remove("g");
    let after = build(&native, &groups, Some("HDMI-A-1"));
    assert_eq!(after.groups[0].modules[0].text, "");
    assert_eq!(after.groups[0].modules[1].text, "alt new second");
    assert_eq!(after.groups[0].modules[1].paged, Some(2));
    assert_eq!(alt["weather"], 1);
    assert_eq!(pages["weather"], 1);
    assert!(child.contains("folded"));
}

#[test]
fn group_collapse_joins_caps_damage_and_width_limits() {
    let cfg = Config::parse(&collapse_config()).unwrap();
    let items = [
        item("a", "aaaaaaaaaaaaaaaaaaaa"),
        item("b", "bbbbbbbbbbbbbbbbbbbb"),
        item("c", "cccccccccccccccccccc"),
    ];
    let native = Registry::new(&Default::default());
    let empty = Default::default();
    let open = compute(
        &cfg,
        &group_inputs(&items, &native, &empty),
        200.0,
        20.0,
        &mut Fixed,
        None,
    );
    for name in ["a", "b", "c"] {
        let groups = [name.to_string()].into();
        let inputs = group_inputs(&items, &native, &groups);
        let folded = compute(&cfg, &inputs, 200.0, 20.0, &mut Fixed, None);
        assert_eq!(folded.groups.len(), 3);
        assert_eq!(folded.group_separators.len(), 2);
        assert_eq!(folded.groups[0].separators.len(), 1);
        assert!(folded.groups[1].separators.is_empty());
        assert_eq!(folded.groups[2].separators.len(), 1);
        for (join, pair) in folded.group_separators.iter().zip(folded.groups.windows(2)) {
            assert_eq!(join.fill, pair[0].modules.last().unwrap().background);
            assert_eq!(join.under, pair[1].modules[0].background);
            assert!(
                folded
                    .click_at(join.x + join.width / 2.0, 10.0, 3)
                    .is_none()
            );
        }
        for (old, new) in [(&open, &folded), (&folded, &open)] {
            let Damage::Rects(rects) = new.damage(old) else {
                panic!("same groups")
            };
            assert!(!rects.is_empty());
            for (before, after) in old.group_separators.iter().zip(&new.group_separators) {
                if before.same_paint(after) {
                    continue;
                }
                for join in [before, after] {
                    assert!(rects.iter().any(|(x, _, w, _)| *x <= join.x - join.overlap
                        && x + w >= join.x + join.width + join.overlap));
                }
            }
        }
        for width in 0..100 {
            let frame = compute(&cfg, &inputs, width as f32, 20.0, &mut Fixed, None);
            for group in &frame.groups {
                assert!(group.x >= 0.0 && group.x + group.width <= width as f32);
                for module in &group.modules {
                    assert!(
                        module.x >= group.x && module.x + module.width <= group.x + group.width
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "release layout timing probe; run with --release --ignored --nocapture"]
fn benchmark_group_collapse_layout() {
    use std::{hint::black_box, time::Instant};
    let enabled = Config::parse(&collapse_config()).unwrap();
    let mut disabled = enabled.clone();
    for group in &mut disabled.positions[2].groups {
        group.collapse = None;
    }
    let items = [
        item("a", "CPU 12%"),
        item("b", "Memory 34%"),
        item("c", "Temperature 45°C"),
    ];
    let native = Registry::new(&Default::default());
    let empty = Default::default();
    let groups = ["a", "b", "c"].map(str::to_string).into();
    let mut text = crate::text::TextRenderer::new(
        &enabled.bar.font_family,
        enabled.bar.font_size,
        &enabled.bar.font_fallback,
    )
    .unwrap();
    for (label, cfg, groups) in [
        ("disabled", &disabled, &empty),
        ("expanded", &enabled, &empty),
        ("collapsed", &enabled, &groups),
    ] {
        let inputs = group_inputs(&items, &native, groups);
        for _ in 0..100 {
            black_box(compute(cfg, &inputs, 1920.0, 30.0, &mut text, None));
        }
        let start = Instant::now();
        for _ in 0..20_000 {
            black_box(compute(cfg, &inputs, 1920.0, 30.0, &mut text, None));
        }
        println!(
            "group layout {label}: {:.2} us/frame",
            start.elapsed().as_secs_f64() * 1e6 / 20_000.0
        );
    }
}

#[test]
fn collapsed_island_respects_padding_caps_and_icon_width_limits() {
    let config = "[bar]\ngap = 0\n[left]\ngroups = ['g']\n[group.g]\nmodules = ['*']\ncollapsible = true\ncollapse_button = 'right'\npadding = 2\nradius = 8\nopacity = 0.7\nends = { left = 'slant', right = 'slant', width = 3 }\ncollapsed = { icon = '$tux', icon_size = 10, padding = 4, min_width = 22 }";
    let cfg = Config::parse(config).unwrap();
    let native = Registry::new(&Default::default());
    let groups = ["g".to_string()].into();
    let inputs = group_inputs(&[], &native, &groups);
    for width in 0..70 {
        let frame = compute(&cfg, &inputs, width as f32, 30.0, &mut Fixed, None);
        if width < 32 {
            assert!(frame.groups.is_empty());
        } else {
            let group = &frame.groups[0];
            assert_eq!(group.width, 32.0);
            assert_eq!(group.opacity, 0.7);
            assert_eq!(group.edges.radius, 8.0);
            assert_eq!(group.separators.len(), 2);
            let icon = group.modules[0].icon.as_ref().unwrap();
            assert_eq!(icon.icon, Icon::Tux);
            assert_eq!(icon.x, 11.0);
            assert_eq!(icon.y, 10.0);
        }
    }
    // A cap the collapsed icon cannot fit into leaves the group nothing to expand it
    // with, so it is refused when the config is read rather than at the first click.
    let capped = Config::parse(&config.replace("min_width = 22", "max_width = 17"));
    let refused = format!("{:#}", capped.expect_err("a cap the icon cannot fit"));
    assert!(
        refused.contains("nothing left to expand it with"),
        "unexpected error: {refused}"
    );
    let items = [item("a", "first"), item("b", "second")];
    let empty = Default::default();
    let expanded = group_inputs(&items, &native, &empty);
    let mut disabled = cfg.clone();
    disabled.positions[0].groups[0].collapse = None;
    let before = compute(&disabled, &expanded, 200.0, 30.0, &mut Fixed, None);
    let enabled = compute(&cfg, &expanded, 200.0, 30.0, &mut Fixed, None);
    assert_eq!(enabled.damage(&before), Damage::Rects(vec![]));
}
/// The config a wording travel is measured against: one module with two wordings of
/// very different lengths, and a second behind it to be pushed about.
const SWITCHING: &str = r##"
[bar]
height = 20
[right]
groups = ["g"]
[group.g]
modules = ["a", "b"]
padding = 0
spacing = 0
[module.a]
format = "$text"
format_alt = "$text spelled out at length"
padding = 2
[module.b]
format = "$text"
padding = 2
"##;

/// A module between two wordings is drawn at neither of them: it leaves the width the
/// settled one had and arrives at the width the new one wants, and everything in
/// between is the travel. Landing anywhere but exactly on those two ends is a jump on
/// the first frame or the last.
#[test]
fn a_wording_on_its_way_is_drawn_between_the_two_widths() {
    let cfg = Config::parse(SWITCHING).unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("a", "42%"), item("b", "x")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let width = |inputs: &Inputs<'_>| {
        compute(&cfg, inputs, 400.0, 20.0, &mut Fixed, None).groups[0].modules[0].width
    };

    let first = width(&inputs);
    let second: std::collections::HashMap<String, usize> = [("a".to_string(), 1)].into();
    inputs.alt = &second;
    let alt = width(&inputs);
    assert!(alt > first, "the second wording is the longer one");

    let steps: Vec<(f32, std::collections::HashMap<String, Leaving>)> =
        [(0.0, first), (0.5, (first + alt) / 2.0), (1.0, alt)]
            .into_iter()
            .map(|(at, want)| (want, [("a".to_string(), Leaving { from: 0, at })].into()))
            .collect();
    for (want, travelling) in &steps {
        inputs.switching = travelling;
        let got = width(&inputs);
        assert!((got - want).abs() < 0.001, "{got} against {want}");
    }
}

/// What a travelling module charges is the wider of its two wordings from end to end
/// of the travel. Charging what it is showing would hand the module behind it a
/// different budget on every frame, and a title there would shed and regain a
/// character at a time all the way through.
#[test]
fn a_wording_on_its_way_measures_its_neighbour_once() {
    let cfg = Config::parse(SWITCHING).unwrap();
    let native = Registry::new(&Default::default());
    let items = [
        item("a", "42%"),
        item("b", "a window title with plenty in it"),
    ];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    // Narrow enough that the module behind has to be cut, which is what makes a
    // budget that moves show up as text that changes.
    let title = |inputs: &Inputs<'_>| {
        compute(&cfg, inputs, 46.0, 20.0, &mut Fixed, None).groups[0].modules[1]
            .text
            .clone()
    };

    let second: std::collections::HashMap<String, usize> = [("a".to_string(), 1)].into();
    inputs.alt = &second;
    let settled = title(&inputs);
    let steps: Vec<(f32, std::collections::HashMap<String, Leaving>)> = [0.0, 0.25, 0.5, 0.75, 1.0]
        .into_iter()
        .map(|at| (at, [("a".to_string(), Leaving { from: 0, at })].into()))
        .collect();
    for (at, travelling) in &steps {
        inputs.switching = travelling;
        let got = title(&inputs);
        assert_eq!(got, settled, "the title was re-cut at {at}");
    }
}

/// A wording is fitted to the width it lands at, so until it lands it is holding more
/// than its box: it is cut at its own edge, short of the fills by the padding nothing
/// is ever written in, and it never hangs off the left into the module before it.
#[test]
fn a_wording_wider_than_its_box_is_cut_at_it() {
    let cfg = Config::parse(SWITCHING).unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("a", "42%"), item("b", "x")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let second: std::collections::HashMap<String, usize> = [("a".to_string(), 1)].into();
    inputs.alt = &second;

    let settled = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    assert_eq!(
        settled.groups[0].modules[0].content_right, None,
        "a module that has arrived is not cut at all"
    );

    let travelling: std::collections::HashMap<String, Leaving> =
        [("a".to_string(), Leaving { from: 0, at: 0.25 })].into();
    inputs.switching = &travelling;
    let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    let module = &frame.groups[0].modules[0];
    let cut = module.content_right.expect("a travelling module is cut");
    assert!((cut - (module.x + module.width - 2.0)).abs() < 0.001);
    assert!(
        module.text_x >= module.x + 2.0 - 0.001,
        "the wording starts inside the module, not in the one before it"
    );
    assert!(
        module.text_x + Fixed.measure(&module.text) > cut,
        "a quarter of the way out it is still holding more than it shows"
    );
}

/// A module folded down to its icon says the same thing in every wording, so there is
/// nothing for a click to travel between and nothing to cut.
#[test]
fn a_folded_module_does_not_travel_between_wordings() {
    let cfg = Config::parse(&SWITCHING.replace(
        "[module.a]",
        "[module.a]\ncollapsible = true\ncollapse_button = \"right\"\nicon = \"$cpu\"",
    ))
    .unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("a", "42%"), item("b", "x")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let folded: std::collections::HashSet<String> = ["a".to_string()].into();
    inputs.collapsed = &folded;

    let settled = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    let travelling: std::collections::HashMap<String, Leaving> =
        [("a".to_string(), Leaving { from: 0, at: 0.5 })].into();
    inputs.switching = &travelling;
    let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    assert_eq!(frame.groups[0].modules[0].content_right, None);
    assert_eq!(
        frame.groups[0].modules[0].width,
        settled.groups[0].modules[0].width
    );
}
/// A state rule can key on what a module says, so the two wordings of a travel can
/// resolve different styles - and padding, min_width and the icon are all metrics. Each
/// end has to be measured in its own, or the frame the click lands on is the wording it
/// is leaving drawn at a width it never had.
#[test]
fn each_wording_of_a_travel_is_measured_in_its_own_style() {
    let config = r##"
[bar]
height = 20
[right]
groups = ["g"]
[group.g]
modules = ["a"]
padding = 0
[module.a]
format = "$text up"
format_alt = "$text"
padding = 2

[module.a.states.loud]
contains = "up"
padding = 10
"##;
    let cfg = Config::parse(config).unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("a", "42")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let width = |inputs: &Inputs<'_>| {
        compute(&cfg, inputs, 400.0, 20.0, &mut Fixed, None).groups[0].modules[0].width
    };

    let first = width(&inputs);
    let second: std::collections::HashMap<String, usize> = [("a".to_string(), 1)].into();
    inputs.alt = &second;
    let alt = width(&inputs);
    assert!(
        first > alt + 10.0,
        "the rule has to move the metrics for this to test anything: {first} and {alt}"
    );

    // Leaving the padded wording behind, on the frame the click landed: exactly the
    // width the settled module had, in the style the rule gave it.
    let travelling: std::collections::HashMap<String, Leaving> =
        [("a".to_string(), Leaving { from: 0, at: 0.0 })].into();
    inputs.switching = &travelling;
    let got = width(&inputs);
    assert!(
        (got - first).abs() < 0.001,
        "the travel started at {got} rather than at {first}"
    );
}

/// A wording that says nothing takes the module off the bar. Clicked on to, that is a
/// width of nothing rather than a module that is suddenly not there: the box travels
/// to it and goes when it arrives, and a click back grows it out of nothing again.
#[test]
fn a_travel_to_a_wording_that_says_nothing_shrinks_the_box_away() {
    let config = r##"
[bar]
height = 20
[right]
groups = ["g"]
[group.g]
modules = ["a"]
padding = 7
ends = { left = "slant", right = "slant", width = 3 }
[module.a]
format = "$text"
format_alt = ""
padding = 2
"##;
    let cfg = Config::parse(config).unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("a", "42")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let frame = |inputs: &Inputs<'_>| compute(&cfg, inputs, 400.0, 20.0, &mut Fixed, None);

    let open_frame = frame(&inputs);
    let open = open_frame.groups[0].modules[0].width;
    let open_group = open_frame.groups[0].width;
    let second: std::collections::HashMap<String, usize> = [("a".to_string(), 1)].into();
    inputs.alt = &second;
    assert!(
        frame(&inputs).groups.is_empty(),
        "a wording that says nothing is a module that is not there"
    );

    // On its way out: the width it had, then half of it, and nothing written on it.
    let steps: Vec<(f32, std::collections::HashMap<String, Leaving>)> = [0.0, 0.5, 1.0]
        .into_iter()
        .map(|at| (at, [("a".to_string(), Leaving { from: 0, at })].into()))
        .collect();
    for (at, travelling) in &steps {
        inputs.switching = travelling;
        let drawn = frame(&inputs);
        let module = &drawn.groups[0].modules[0];
        assert!(module.text.is_empty(), "at {at}");
        assert!(
            (module.width - open * (1.0 - at)).abs() < 0.001,
            "at {at}: {} against {}",
            module.width,
            open * (1.0 - at)
        );
        assert!(
            (drawn.groups[0].width - open_group * (1.0 - at)).abs() < 0.001,
            "the island's padding and caps jumped at {at}: {} against {}",
            drawn.groups[0].width,
            open_group * (1.0 - at)
        );
    }

    // And the way back: out of nothing rather than in at full width.
    let showing: std::collections::HashMap<String, usize> = Default::default();
    inputs.alt = &showing;
    let back: std::collections::HashMap<String, Leaving> =
        [("a".to_string(), Leaving { from: 1, at: 0.5 })].into();
    inputs.switching = &back;
    let grown = frame(&inputs);
    let module = &grown.groups[0].modules[0];
    assert!((module.width - open / 2.0).abs() < 0.001);
    assert!((grown.groups[0].width - open_group / 2.0).abs() < 0.001);
}

/// A disappearing module takes one of the gaps around it away. The other becomes the
/// ordinary gap between the modules that remain, so neither spacing nor a configured
/// separator can stay at full width and then vanish on the last frame.
#[test]
fn a_hidden_wording_travels_with_its_inter_module_gap() {
    let config = r##"
[bar]
height = 20
[left]
groups = ["g"]
[group.g]
modules = ["a", "b", "c"]
padding = 0
[group.g.separator]
shape = "slant"
width = 10
overlap = 2
[module.a]
format = "$text"
padding = 0
[module.b]
format = "$text"
format_alt = ""
padding = 0
[module.c]
format = "$text"
padding = 0
"##;
    let cfg = Config::parse(config).unwrap();
    let native = Registry::new(&Default::default());
    let items = [item("a", "a"), item("b", "b"), item("c", "c")];
    let none = Default::default();
    let mut inputs = group_inputs(&items, &native, &none);
    let open = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    let open_width = open.groups[0].width;

    let showing: std::collections::HashMap<String, usize> = [("b".to_string(), 1)].into();
    inputs.alt = &showing;
    let closed = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    let closed_width = closed.groups[0].width;
    assert!((open_width - closed_width - 11.0).abs() < 0.001);

    let steps: Vec<_> = [0.0, 0.5, 1.0]
        .into_iter()
        .map(|at| (at, [("b".to_string(), Leaving { from: 0, at })].into()))
        .collect();
    for (at, travelling) in &steps {
        inputs.switching = travelling;
        let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
        let want = open_width + (closed_width - open_width) * *at;
        assert!(
            (frame.groups[0].width - want).abs() < 0.001,
            "the gap jumped at {at}: {} against {want}",
            frame.groups[0].width
        );
        let separators = &frame.groups[0].separators;
        if *at < 1.0 {
            assert_eq!(separators.len(), 2);
            assert!((separators[0].width - 10.0 * (1.0 - at)).abs() < 0.001);
            assert!((separators[0].overlap - 2.0 * (1.0 - at)).abs() < 0.001);
        } else {
            assert_eq!(separators.len(), 1);
        }
        assert!((separators.last().unwrap().width - 10.0).abs() < 0.001);
    }
}

/// A glyph in the icon slot is wording, and it leads the format the way geometry leads it.
///
/// The stub measurer makes every character one unit, so the module is the glyph, the space
/// that separates it from the wording, and the three characters of the wording itself.
#[test]
fn a_written_icon_is_shaped_in_front_of_the_wording() {
    assert_eq!(width_of("padding = 0"), 3.0);
    assert_eq!(width_of("padding = 0\nicon = \"\u{f0e7}\""), 5.0);
    // Nothing is charged for `icon_size` or the gap: a glyph is not geometry, so neither
    // of the two knobs that place geometry has anything to say about it.
    assert_eq!(
        width_of("padding = 0\nicon = \"\u{f0e7}\"\nicon_size = 40\nicon_gap = 9"),
        5.0
    );
}

/// Folding a module with a written icon leaves the glyph, which is the whole reason a
/// glyph is allowed in the slot: a module whose icon is text could not be folded before.
#[test]
fn a_module_folds_down_to_a_written_icon() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]
padding = 0
spacing = 0

[module.cpu]
padding = 0
collapsible = true
icon = "µ"
"##;
    let items = [item("cpu", "abc")];
    let open = frame_of(config, &items);
    assert_eq!(open.groups[0].modules[0].width, 5.0);
    assert_eq!(open.groups[0].modules[0].text, "\u{b5}\u{20}abc");

    let folded: std::collections::HashSet<String> = ["cpu".to_string()].into();
    let shut = frame_folded(
        config,
        &items,
        Registry::new(&Default::default()),
        &Default::default(),
        &folded,
    );
    // The glyph alone, with no icon in the geometry slot and no space left behind it.
    assert_eq!(shut.groups[0].modules[0].width, 1.0);
    assert_eq!(shut.groups[0].modules[0].text, "\u{b5}");
    assert!(shut.groups[0].modules[0].icon.is_none());
}

/// A module folding is drawn at neither of its two widths but between them, and holds
/// more than it shows while it is there.
#[test]
fn a_module_folding_eases_between_its_two_widths() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]
padding = 0
spacing = 0

[module.cpu]
padding = 0
collapsible = true
collapse_animation = "150ms"
icon = "$cpu"
icon_size = 10
icon_gap = 0
"##;
    let items = [item("cpu", "abc")];
    let native = Registry::new(&Default::default());
    let no_groups: std::collections::HashSet<String> = Default::default();

    // The icon, and three characters of wording the stub measures at one unit each.
    let open = frame_of(config, &items);
    assert_eq!(open.groups[0].modules[0].width, 13.0);
    // Folded, the wording is gone and the gap goes with it.
    let shut_set: std::collections::HashSet<String> = ["cpu".to_string()].into();
    let shut = frame_folded(
        config,
        &items,
        Registry::new(&Default::default()),
        &Default::default(),
        &shut_set,
    );
    assert_eq!(shut.groups[0].modules[0].width, 10.0);

    // Half way is half way between the two, whichever end it is heading for, and the
    // module is cut at its own edge because it was fitted to neither width.
    for shutting in [false, true] {
        let cfg = Config::parse(config).unwrap();
        let mut inputs = group_inputs(&items, &native, &no_groups);
        let settled: std::collections::HashSet<String> = match shutting {
            true => ["cpu".to_string()].into(),
            false => Default::default(),
        };
        let folding: std::collections::HashMap<String, f32> = [("cpu".to_string(), 0.5)].into();
        inputs.collapsed = &settled;
        inputs.module_folding = &folding;
        let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
        let module = &frame.groups[0].modules[0];
        assert_eq!(module.width, 11.5, "shutting = {shutting}");
        assert!(module.content_right.is_some(), "shutting = {shutting}");
    }
}

/// A module that names a collapsed style wears it folded, icon and all, so the thing left
/// on the bar can say something the open module does not.
#[test]
fn a_module_folds_into_the_style_it_named() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]
padding = 0
spacing = 0

[module.cpu]
padding = 0
collapsible = true
icon = "$cpu"
icon_size = 10
icon_gap = 0
foreground = "#ffffff"

[module.cpu.collapsed]
icon = "$memory"
foreground = "#ff0000"
"##;
    let items = [item("cpu", "abc")];
    let open = frame_of(config, &items);
    assert_eq!(
        open.groups[0].modules[0].icon.as_ref().unwrap().icon,
        crate::icon::Icon::parse("cpu").unwrap()
    );
    assert_eq!(
        open.groups[0].modules[0].foreground,
        Color::parse("#ffffff").unwrap()
    );

    let shut_set: std::collections::HashSet<String> = ["cpu".to_string()].into();
    let shut = frame_folded(
        config,
        &items,
        Registry::new(&Default::default()),
        &Default::default(),
        &shut_set,
    );
    assert_eq!(
        shut.groups[0].modules[0].icon.as_ref().unwrap().icon,
        crate::icon::Icon::parse("memory").unwrap()
    );
    assert_eq!(
        shut.groups[0].modules[0].foreground,
        Color::parse("#ff0000").unwrap()
    );
}

/// Hovering a module that wears a written collapsed style leaves it wearing it.
///
/// The collapsed table stands in place of the state rules, and hover is a state rule, so
/// there is nothing for the pointer to resolve. Before this was so, hover was compared
/// against a cascade the module was not wearing, matched on nearly every key, and handed
/// back the open colours - which took the collapsed ones away for as long as the pointer
/// was over the one thing left to click.
#[test]
fn hovering_a_folded_module_keeps_the_collapsed_colours() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]
padding = 0
spacing = 0

[module.cpu]
padding = 0
collapsible = true
icon = "$cpu"
icon_size = 10
icon_gap = 0
foreground = "#ffffff"

[module.cpu.collapsed]
icon = "$memory"
foreground = "#ff0000"
"##;
    let cfg = Config::parse(config).unwrap();
    let items = [item("cpu", "abc")];
    let native = Registry::new(&Default::default());
    let no_groups: std::collections::HashSet<String> = Default::default();
    let folded: std::collections::HashSet<String> = ["cpu".to_string()].into();
    let mut inputs = group_inputs(&items, &native, &no_groups);
    inputs.collapsed = &folded;

    let away = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
    let module = &away.groups[0].modules[0];
    assert_eq!(module.foreground, Color::parse("#ff0000").unwrap());

    // The very middle of the module, so the pointer is unambiguously on it.
    let at = (
        module.x + module.width / 2.0,
        module.y + module.height / 2.0,
    );
    let over = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, Some(at));
    assert_eq!(
        over.groups[0].modules[0].foreground,
        Color::parse("#ff0000").unwrap(),
        "the pointer took the collapsed colours away"
    );
    assert_eq!(
        over.groups[0].modules[0].icon.as_ref().unwrap().icon,
        crate::icon::Icon::parse("memory").unwrap(),
        "the pointer took the collapsed icon away"
    );
}

/// A fold shows the module's own contents all the way through, whichever way it is going.
///
/// What a fold moves is the module's edge, not what is written inside it - the same rule a
/// group fold already follows. Drawing the folded shape while opening held the collapsed
/// icon until the last frame and then jumped to the wording.
#[test]
fn a_folding_module_draws_its_open_contents_until_it_arrives() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]
padding = 0
spacing = 0

[module.cpu]
padding = 0
collapsible = true
collapse_animation = "150ms"
icon = "$cpu"
icon_size = 10
icon_gap = 0

[module.cpu.collapsed]
icon = "$memory"
"##;
    let cfg = Config::parse(config).unwrap();
    let items = [item("cpu", "abc")];
    let native = Registry::new(&Default::default());
    let no_groups: std::collections::HashSet<String> = Default::default();

    // Half way, heading each way: shutting is settled shut already, opening is not.
    for (shutting, at) in [(true, 0.5f32), (false, 0.5f32)] {
        let settled: std::collections::HashSet<String> = match shutting {
            true => ["cpu".to_string()].into(),
            false => Default::default(),
        };
        let folding: std::collections::HashMap<String, f32> = [("cpu".to_string(), at)].into();
        let mut inputs = group_inputs(&items, &native, &no_groups);
        inputs.collapsed = &settled;
        inputs.module_folding = &folding;
        let frame = compute(&cfg, &inputs, 400.0, 20.0, &mut Fixed, None);
        let module = &frame.groups[0].modules[0];
        assert_eq!(
            module.icon.as_ref().unwrap().icon,
            crate::icon::Icon::parse("cpu").unwrap(),
            "shutting = {shutting}: the collapsed icon was drawn mid-travel"
        );
        assert_eq!(
            module.text, "abc",
            "shutting = {shutting}: the wording was taken away mid-travel"
        );
    }
}

/// A folded module is the one thing left to click, so it is never fitted away.
///
/// A glyph cannot be measured when the config is read - there are no fonts yet - so a
/// `max_width` too small for it gets past validation. Truncating it to nothing then hid
/// the module and took its own expand target with it.
#[test]
fn a_folded_module_survives_a_max_width_too_small_for_its_icon() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]
padding = 0
spacing = 0

[module.cpu]
padding = 0
collapsible = true
icon = "µ"
max_width = 0.5
"##;
    let items = [item("cpu", "abc")];
    let folded: std::collections::HashSet<String> = ["cpu".to_string()].into();
    let frame = frame_folded(
        config,
        &items,
        Registry::new(&Default::default()),
        &Default::default(),
        &folded,
    );
    let module = frame.groups[0]
        .modules
        .first()
        .expect("a folded module keeps something to click on");
    assert!(module.width > 0.0, "nothing left to click");
    assert_eq!(module.text, "\u{b5}");
}

/// A fold shows the icon the module is actually wearing, which is the matching state's.
///
/// A module that swaps its icon per state - a player showing pause while it plays and play
/// while it is paused - folds to the icon for what it is doing now. Naming one in the
/// `collapsed` table overrides that; leaving the table out, or writing one that says
/// nothing about the icon, lets the state through.
#[test]
fn a_fold_takes_the_icon_of_the_state_the_module_is_in() {
    let module = |collapsed: &str| {
        format!(
            r##"
[left]
groups = ["g"]

[group.g]
modules = ["m"]
padding = 0
spacing = 0

[module.m]
padding = 0
collapsible = true
icon = "$play"
icon_size = 10
icon_gap = 0

[module.m.states.busy]
state = "warning"
icon = "$pause"
{collapsed}
"##
        )
    };
    let folded: std::collections::HashSet<String> = ["m".to_string()].into();
    let warned = |text: &str| {
        let mut it = item("m", text);
        it.state = State::Warning;
        it
    };

    // No collapsed table: the state's icon is what is left on the bar.
    for (case, state_item, want) in [
        ("idle", item("m", "abc"), "play"),
        ("warning", warned("abc"), "pause"),
    ] {
        let frame = frame_folded(
            &module(""),
            &[state_item],
            Registry::new(&Default::default()),
            &Default::default(),
            &folded,
        );
        assert_eq!(
            frame.groups[0].modules[0].icon.as_ref().unwrap().icon,
            crate::icon::Icon::parse(want).unwrap(),
            "{case}: the fold did not take the state's icon"
        );
    }

    // A collapsed table that says nothing about the icon still lets the state through.
    let coloured = "\n[module.m.collapsed]\nforeground = \"#ff0000\"\n";
    let frame = frame_folded(
        &module(coloured),
        &[warned("abc")],
        Registry::new(&Default::default()),
        &Default::default(),
        &folded,
    );
    assert_eq!(
        frame.groups[0].modules[0].icon.as_ref().unwrap().icon,
        crate::icon::Icon::parse("pause").unwrap(),
        "a collapsed table with no icon should not take the state's away"
    );
    assert_eq!(
        frame.groups[0].modules[0].foreground,
        Color::parse("#ff0000").unwrap(),
        "the collapsed table's own keys still apply"
    );

    // One it does name wins over every state.
    let named = "\n[module.m.collapsed]\nicon = \"$media\"\n";
    let frame = frame_folded(
        &module(named),
        &[warned("abc")],
        Registry::new(&Default::default()),
        &Default::default(),
        &folded,
    );
    assert_eq!(
        frame.groups[0].modules[0].icon.as_ref().unwrap().icon,
        crate::icon::Icon::parse("media").unwrap(),
        "a named collapsed icon should win"
    );
}
