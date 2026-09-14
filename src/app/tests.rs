use super::*;
use crate::config::ClickActions;

#[test]
fn a_submenu_flips_across_its_row_at_a_horizontal_screen_edge() {
    let root = menu_constraints(false);
    assert!(root.contains(xdg_positioner::ConstraintAdjustment::SlideX));
    assert!(!root.contains(xdg_positioner::ConstraintAdjustment::FlipX));

    let submenu = menu_constraints(true);
    assert!(submenu.contains(xdg_positioner::ConstraintAdjustment::FlipX));
    assert!(submenu.contains(xdg_positioner::ConstraintAdjustment::SlideX));
    assert!(submenu.contains(xdg_positioner::ConstraintAdjustment::FlipY));
    assert!(submenu.contains(xdg_positioner::ConstraintAdjustment::SlideY));
}

#[test]
fn only_an_enabled_submenu_is_opened_by_hover() {
    let row = |id, enabled, submenu| crate::tray::menu::Row {
        id,
        enabled,
        submenu,
        ..Default::default()
    };
    let rows = vec![row(1, true, false), row(2, false, true), row(3, true, true)];

    assert_eq!(pointed_submenu(&rows, Some(0)), None);
    assert_eq!(pointed_submenu(&rows, Some(1)), None);
    assert_eq!(pointed_submenu(&rows, Some(2)), Some(3));
    assert_eq!(pointed_submenu(&rows, None), None);
}

#[test]
fn an_open_or_in_flight_submenu_is_not_opened_again() {
    assert!(submenu_is_current(Some(7), None, 7));
    assert!(submenu_is_current(None, Some((42, 7)), 7));
    assert!(!submenu_is_current(Some(6), None, 7));
    assert!(!submenu_is_current(None, Some((42, 6)), 7));
}

#[test]
fn an_in_flight_submenu_gets_the_same_leave_grace_as_a_mapped_one() {
    assert!(submenu_leave_needs_grace(0, false, true));
    assert!(submenu_leave_needs_grace(0, true, false));
    assert!(submenu_leave_needs_grace(1, false, false));
    assert!(!submenu_leave_needs_grace(0, false, false));
}

/// A placed module with nothing on it, for saying what one gesture key does without
/// describing a whole bar.
fn placed() -> PlacedModule {
    PlacedModule {
        x: 0.0,
        y: 0.0,
        width: 10.0,
        height: 10.0,
        icon: None,
        text: String::new(),
        text_x: 0.0,
        content_right: None,
        text_right: None,
        foreground: crate::color::Color::TRANSPARENT,
        background: crate::color::Color::TRANSPARENT,
        radius: 0.0,
        action: None,
        name: Some("weather".to_string()),
        alt: None,
        alt_button: Button::Left,
        refresh: None,
        mute: None,
        paged: None,
        collapsible: false,
        collapse_button: Button::Right,
        on_click: None,
    }
}

/// The wheel turns pages on a module that has them. It used to be answered by
/// whatever the module's buttons did, because a notch has no button to compare.
#[test]
fn a_notch_turns_a_page_and_says_which_way() {
    let module = PlacedModule {
        paged: Some(3),
        refresh: Some(Button::Left),
        ..placed()
    };
    assert_eq!(
        gesture(&module, SCROLL_DOWN),
        Gesture::Page {
            count: 3,
            forward: true
        }
    );
    assert_eq!(
        gesture(&module, SCROLL_UP),
        Gesture::Page {
            count: 3,
            forward: false
        }
    );
    assert_eq!(gesture(&module, Button::Left.number()), Gesture::Refresh);
}

/// A module with nothing to page through leaves the notch alone, and it goes on to
/// whatever the module is showing - a volume that scrolls, or a provider's block.
#[test]
fn a_notch_on_a_module_with_one_reading_is_left_alone() {
    let module = PlacedModule {
        refresh: Some(Button::Left),
        ..placed()
    };
    assert_eq!(gesture(&module, SCROLL_UP), Gesture::Forward);
    assert_eq!(gesture(&module, SCROLL_DOWN), Gesture::Forward);
}

/// A program of the user's own is the one thing the config asked for outright, so it
/// takes its button before anything built in looks at it.
#[test]
fn a_program_of_your_own_comes_before_anything_built_in() {
    let actions = ClickActions {
        left: Some(vec!["cal".to_string()]),
        ..ClickActions::default()
    };
    let module = PlacedModule {
        on_click: Some(std::sync::Arc::new(actions)),
        alt: Some(2),
        ..placed()
    };
    assert_eq!(
        gesture(&module, Button::Left.number()),
        Gesture::Run(Button::Left)
    );
    // The button it was not given still does what the module says.
    assert_eq!(gesture(&module, Button::Right.number()), Gesture::Forward);
}

#[test]
fn the_wordings_and_the_folding_answer_to_their_own_buttons() {
    let module = PlacedModule {
        alt: Some(3),
        collapsible: true,
        ..placed()
    };
    assert_eq!(gesture(&module, Button::Left.number()), Gesture::Alt(3));
    assert_eq!(gesture(&module, Button::Right.number()), Gesture::Collapse);
    assert_eq!(gesture(&module, Button::Middle.number()), Gesture::Forward);
}

/// A module the frame did not name has nothing remembered against it, so every press
/// goes to what it is showing.
#[test]
fn a_module_no_gesture_names_forwards_everything() {
    let module = PlacedModule {
        name: None,
        alt: Some(2),
        collapsible: true,
        ..placed()
    };
    for button in [1, 2, 3, SCROLL_UP, SCROLL_DOWN] {
        assert_eq!(gesture(&module, button), Gesture::Forward);
    }
}

fn wheel(value120: i32) -> AxisScroll {
    AxisScroll {
        value120,
        ..AxisScroll::default()
    }
}

fn finger(pixels: f64) -> AxisScroll {
    AxisScroll {
        absolute: pixels,
        ..AxisScroll::default()
    }
}

/// One notch is one step, however many events the wheel takes to report it. A
/// high-resolution wheel sends eighths of a notch, and eight of those are one step
/// rather than eight.
#[test]
fn a_wheel_notch_is_one_step_however_finely_it_arrives() {
    let mut carried = 0.0;
    assert_eq!(steps_of(&wheel(120), &mut carried), 1);

    let mut carried = 0.0;
    let stepped: i32 = (0..8).map(|_| steps_of(&wheel(15), &mut carried)).sum();
    assert_eq!(stepped, 1, "eight eighths of a notch made {stepped} steps");
}

/// A touchpad reports pixels and no notches at all, which is what used to turn a
/// two-finger drag into an adjustment per frame.
#[test]
fn a_finger_has_to_travel_before_anything_moves() {
    let mut carried = 0.0;
    for _ in 0..4 {
        assert_eq!(steps_of(&finger(3.0), &mut carried), 0, "moved too early");
    }
    assert_eq!(steps_of(&finger(3.0), &mut carried), 1);
}

/// Scrolling the other way starts from nothing, so a nudge back does not land on a
/// step that the previous direction had almost paid for.
#[test]
fn turning_around_drops_what_was_carried() {
    let mut carried = 0.0;
    // Three quarters of a notch up: not a step yet, but carried.
    assert_eq!(steps_of(&wheel(90), &mut carried), 0);
    // A full notch down is a full step down. Had the upward remainder still been
    // there it would have paid for three quarters of this one, and nothing would
    // have moved.
    assert_eq!(steps_of(&wheel(-120), &mut carried), -1);
}

/// A fast scroll is still every step it asked for, rather than one.
#[test]
fn a_flick_is_worth_every_step_in_it() {
    let mut carried = 0.0;
    assert_eq!(steps_of(&wheel(600), &mut carried), 5);
}

/// The end of a kinetic scroll leaves nothing behind to leak into the next one.
#[test]
fn the_end_of_a_scroll_clears_what_was_carried() {
    let mut carried = 0.0;
    assert_eq!(steps_of(&finger(10.0), &mut carried), 0);
    let stop = AxisScroll {
        stop: true,
        ..AxisScroll::default()
    };
    assert_eq!(steps_of(&stop, &mut carried), 0);
    assert_eq!(carried, 0.0);
    assert_eq!(
        steps_of(&finger(10.0), &mut carried),
        0,
        "carried across a stop"
    );
}

#[test]
fn group_button_precedes_every_child_gesture_and_forwarded_action() {
    use crate::layout::{ClickTarget, PlacedGroup};
    use crate::status::Control;
    let actions = [
        None,
        Some(ActionTarget::Control {
            what: Control::Volume,
            step: 5.0,
        }),
        Some(ActionTarget::Control {
            what: Control::Brightness,
            step: 5.0,
        }),
        Some(ActionTarget::Control {
            what: Control::Media,
            step: 0.0,
        }),
        Some(ActionTarget::Tray { key: "tray".into() }),
        Some(ActionTarget::I3Bar {
            name: Some("block".into()),
            instance: None,
        }),
        Some(ActionTarget::Sway("workspace 1".into())),
    ];
    let modules = [
        PlacedModule {
            on_click: Some(std::sync::Arc::new(ClickActions {
                left: Some(vec!["true".into()]),
                middle: Some(vec!["true".into()]),
                right: Some(vec!["true".into()]),
            })),
            ..placed()
        },
        PlacedModule {
            collapsible: true,
            ..placed()
        },
        PlacedModule {
            alt: Some(3),
            ..placed()
        },
        PlacedModule {
            refresh: Some(Button::Left),
            ..placed()
        },
        PlacedModule {
            mute: Some(Button::Middle),
            ..placed()
        },
        PlacedModule {
            paged: Some(3),
            ..placed()
        },
        placed(),
    ];
    for reserved in [Button::Left, Button::Middle, Button::Right] {
        for module in &modules {
            for action in &actions {
                let mut child = module.clone();
                child.x = 5.0;
                child.y = 2.0;
                child.action = action.clone();
                let mut next = child.clone();
                next.x = 20.0;
                let group = PlacedGroup {
                    collapse: Some(("system".into(), reserved)),
                    content_right: None,
                    text_right: None,
                    content_edge: None,
                    x: 0.0,
                    y: 0.0,
                    width: 40.0,
                    height: 16.0,
                    background: crate::color::Color::TRANSPARENT,
                    opacity: 1.0,
                    edges: crate::geometry::Edges {
                        left: crate::config::EdgeShape::Round,
                        right: crate::config::EdgeShape::Round,
                        radius: 4.0,
                    },
                    modules: vec![child, next],
                    separators: vec![],
                };
                let frame = Frame {
                    groups: vec![group],
                    ..Frame::default()
                };
                // Both children, horizontal/vertical padding, and the internal gap.
                for x in [0.0, 6.0, 16.0, 21.0, 39.0] {
                    for y in [0.0, 5.0, 15.0] {
                        assert!(matches!(
                            frame.click_at(x, y, reserved.number()),
                            Some(ClickTarget::Group("system"))
                        ));
                    }
                }
                for button in 1..=5 {
                    if button == reserved.number() {
                        continue;
                    }
                    let Some(ClickTarget::Module(hit)) = frame.click_at(6.0, 5.0, button) else {
                        panic!("lost child button {button}")
                    };
                    assert!(std::ptr::eq(hit, &frame.groups[0].modules[0]));
                    assert_eq!(gesture(hit, button), gesture(module, button));
                    assert!(frame.click_at(16.0, 5.0, button).is_none());
                }
                assert!(frame.click_at(40.0, 5.0, reserved.number()).is_none());
            }
        }
    }
}
