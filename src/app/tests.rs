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

#[test]
fn a_fold_eases_between_its_two_ends_and_stops_at_them() {
    let started = std::time::Instant::now();
    let over = std::time::Duration::from_millis(200);
    let fold = Fold {
        from: 0.0,
        to: 1.0,
        started,
        over,
    };
    assert_eq!(fold.at(started), 0.0);
    assert_eq!(fold.at(started + over), 1.0);
    // Past the end it stays there rather than running on, which is what makes the
    // frame a fold arrives on the same picture as every frame after it.
    assert_eq!(fold.at(started + over * 3), 1.0);
    assert!(fold.arrived(started + over) && !fold.arrived(started + over / 2));

    // Eased: slow away from each end and quickest through the middle.
    let quarter = fold.at(started + over / 4);
    let half = fold.at(started + over / 2);
    assert!((half - 0.5).abs() < 0.001);
    assert!(quarter < 0.25, "a fold leaves slowly, got {quarter}");

    // The other way round is the same curve read backwards.
    let back = Fold {
        from: 1.0,
        to: 0.0,
        started,
        over,
    };
    assert!((back.at(started + over / 4) - (1.0 - quarter)).abs() < 0.001);

    // Turned around half way, it carries on from where it had got to.
    let turned = Fold {
        from: half,
        to: 0.0,
        started,
        over,
    };
    assert_eq!(turned.at(started), half);
    assert_eq!(turned.at(started + over), 0.0);
}

/// The timer that moves travels along exists only while something is travelling, which
/// is the whole of what keeps a bar with nobody clicking on it at idle. Everything
/// that could leave it running - a claim nobody honoured, a fold that never arrives,
/// a turn that never settles - is a bar spinning a 16 ms timer forever.
#[test]
fn travels_run_a_timer_only_while_something_is_travelling() {
    let over = std::time::Duration::from_millis(200);
    let mut travels = Travels::default();

    // Nothing travelling, so there is no job to claim and nothing to step.
    assert!(!travels.claim());
    let (travelling, changed) = travels.step(std::time::Instant::now());
    assert!(!travelling && !changed);

    travels.folds.turn("a".to_string(), 1.0, over);
    assert_eq!(travels.folds.at.get("a").copied(), Some(0.0));
    // Claimed once and then not again, so two timers never step one fold together.
    assert!(travels.claim());
    assert!(!travels.claim());

    let started = travels.folds.travelling["a"].started;
    let (travelling, changed) = travels.step(started + over / 2);
    assert!(travelling && changed);
    assert!((travels.folds.at["a"] - 0.5).abs() < 0.001);

    // Arriving takes the group out of both maps, asks for one last draw, and stops.
    let (travelling, changed) = travels.step(started + over);
    assert!(
        !travelling && changed,
        "the frame a fold arrives on is a change"
    );
    assert!(travels.folds.travelling.is_empty() && travels.folds.at.is_empty());
    // And having stopped, the claim is free for the next click rather than held by a
    // timer that has already dropped itself.
    assert!(!travels.scheduled);
    travels.folds.turn("a".to_string(), 0.0, over);
    assert!(travels.claim());
}

/// One timer serves a fold and a wording alike, so a module set travelling while an
/// island already is joins the timer that is running rather than starting a second.
#[test]
fn a_wording_and_a_fold_share_one_timer() {
    let over = std::time::Duration::from_millis(200);
    let mut travels = Travels::default();

    // A wording on its own is enough to want the timer.
    travels.wordings.start("m".to_string(), 0, over);
    assert!(travels.claim());
    travels.folds.turn("a".to_string(), 1.0, over);
    assert!(!travels.claim(), "the running timer moves both");

    let started = travels.wordings.travelling["m"].started;
    let (travelling, changed) = travels.step(started + over / 2);
    assert!(travelling && changed);
    assert!((travels.wordings.at["m"].at - 0.5).abs() < 0.001);
    assert_eq!(travels.wordings.at["m"].from, 0);

    // Both arrive, and the timer stops for the two of them at once.
    let (travelling, _) = travels.step(started + over * 2);
    assert!(!travelling);
    assert!(travels.wordings.travelling.is_empty() && travels.wordings.at.is_empty());
}

/// A wording is a whole travel from a standing start every time: a module clicked
/// again half way is going somewhere else, and there is no half-way between three
/// wordings to carry on from.
#[test]
fn a_wording_clicked_again_starts_over_from_the_one_it_was_heading_for() {
    let over = std::time::Duration::from_millis(200);
    let mut travels = Travels::default();
    travels.wordings.start("m".to_string(), 0, over);
    let started = travels.wordings.travelling["m"].started;
    travels.step(started + over / 2);

    travels.wordings.start("m".to_string(), 1, over);
    let leaving = travels.wordings.at["m"];
    assert_eq!(leaving.from, 1);
    assert_eq!(leaving.at, 0.0);
    assert!(!travels.wordings.travelling["m"].arrived(started + over));
}

/// A claim that could not be honoured has to be given back. Nothing else drains the
/// travels, so a claim left standing is every island afterwards frozen half shut.
#[test]
fn a_claim_nothing_honoured_is_given_back() {
    let mut travels = Travels::default();
    travels
        .folds
        .turn("a".to_string(), 1.0, std::time::Duration::from_millis(200));
    assert!(travels.claim());
    travels.release();
    assert!(travels.claim(), "a released claim can be taken again");
}

/// A fold turned around covers what is left of its range, not the whole of it. Caught
/// a tenth of the way out, it is a tenth of the way back - and giving that the full
/// configured span is an island that sits at nearly its open width for a quarter of a
/// second while the settled state already says it is open.
#[test]
fn a_fold_turned_around_is_given_the_time_the_distance_is_worth() {
    let over = std::time::Duration::from_millis(200);
    let mut folds = Folds::default();
    folds.turn("a".to_string(), 1.0, over);
    let started = folds.travelling["a"].started;
    folds.step(started + over / 10);
    let caught = folds.at["a"];
    assert!(
        caught > 0.0 && caught < 0.1,
        "eased away slowly, got {caught}"
    );

    folds.turn("a".to_string(), 0.0, over);
    let back = &folds.travelling["a"];
    assert_eq!(back.from, caught);
    assert_eq!(back.to, 0.0);
    assert!(
        back.over < over / 10,
        "a fold covering {caught} of its range was given {:?} of {over:?}",
        back.over
    );

    // Clicked twice before the first step, there is nothing left to cover: it arrives
    // at once rather than holding the timer open for a range of zero.
    let mut folds = Folds::default();
    folds.turn("b".to_string(), 1.0, over);
    folds.turn("b".to_string(), 0.0, over);
    assert_eq!(folds.travelling["b"].from, folds.travelling["b"].to);
    let now = std::time::Instant::now();
    assert!(folds.travelling["b"].arrived(now));
    let (travelling, changed) = folds.step(now);
    assert!(!travelling && changed);
}

/// A group whose config stopped asking for an animation cannot be left with one in
/// flight: it would never be stepped to an end, because layout would keep reading a
/// half-shut width that nothing owns.
#[test]
fn a_group_that_stopped_animating_is_settled_rather_than_left_travelling() {
    let mut folds = Folds::default();
    folds.turn("a".to_string(), 1.0, std::time::Duration::from_millis(200));
    folds.turn("b".to_string(), 1.0, std::time::Duration::from_millis(200));
    folds.settle("a");
    assert!(!folds.travelling.contains_key("a") && !folds.at.contains_key("a"));
    assert!(folds.travelling.contains_key("b") && folds.at.contains_key("b"));
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
