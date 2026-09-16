//! Runtime state for the bar's bounded fold and wording animations, and for hiding it.
//!
//! This module owns the state and timestamps. The event loop owns when the shared timer
//! runs, keeping animation mechanics and scheduling out of layout.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::layout::Leaving;

/// One value on its way between two ends.
struct Fold {
    from: f32,
    to: f32,
    started: Instant,
    over: Duration,
}

impl Fold {
    /// Where the fold has got to, eased so it leaves and arrives slowly.
    fn at(&self, now: Instant) -> f32 {
        let over = self.over.as_secs_f32();
        let gone = now.saturating_duration_since(self.started).as_secs_f32();
        let linear = match over > 0.0 {
            true => (gone / over).clamp(0.0, 1.0),
            false => 1.0,
        };
        let eased = linear * linear * (3.0 - 2.0 * linear);
        self.from + (self.to - self.from) * eased
    }

    fn arrived(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.over
    }
}

/// Every group or module on its way between open and shut.
#[derive(Default)]
struct Folds {
    travelling: HashMap<String, Fold>,
    /// Current positions shown to layout, by configured name.
    at: HashMap<String, f32>,
}

impl Folds {
    /// Send one fold towards `to`, from wherever it had got to.
    fn turn(&mut self, name: String, to: f32, over: Duration) {
        let from = self.at.get(&name).copied().unwrap_or(1.0 - to);
        let span = (from - to).abs();
        // A whole travel is the configured length exactly: scaling by one would round it
        // through an f32 and hand back something a nanosecond either side of it.
        let over = match span < 1.0 {
            true => over.mul_f32(span),
            false => over,
        };
        self.travelling.insert(
            name.clone(),
            Fold {
                from,
                to,
                started: Instant::now(),
                over,
            },
        );
        self.at.insert(name, from);
    }

    /// Forget a fold whose configuration no longer asks for animation.
    fn settle(&mut self, name: &str) {
        self.travelling.remove(name);
        self.at.remove(name);
    }

    /// Move every fold along, returning whether any remain and whether the frame changed.
    fn step(&mut self, now: Instant) -> (bool, bool) {
        let was = self.travelling.len();
        let Folds { travelling, at } = self;
        // Arriving removes a fold entirely, returning layout to its settled fast path.
        travelling.retain(|_, fold| !fold.arrived(now));
        at.retain(|name, _| travelling.contains_key(name));
        for (name, fold) in travelling.iter() {
            if let Some(at) = at.get_mut(name) {
                *at = fold.at(now);
            }
        }
        let going = !travelling.is_empty();
        (going, going || was > 0)
    }

    fn idle(&self) -> bool {
        self.travelling.is_empty()
    }
}

/// Every module moving from one wording to the next.
#[derive(Default)]
struct Wordings {
    travelling: HashMap<String, Fold>,
    /// The wording being left and the current position, by configured module name.
    at: HashMap<String, Leaving>,
}

impl Wordings {
    /// Start a whole travel from the wording the click just left.
    fn start(&mut self, name: String, from: usize, over: Duration) {
        self.travelling.insert(
            name.clone(),
            Fold {
                from: 0.0,
                to: 1.0,
                started: Instant::now(),
                over,
            },
        );
        self.at.insert(name, Leaving { from, at: 0.0 });
    }

    /// Forget a wording transition whose configuration no longer asks for animation.
    fn settle(&mut self, name: &str) {
        self.travelling.remove(name);
        self.at.remove(name);
    }

    /// Move every wording along, returning whether any remain and whether the frame changed.
    fn step(&mut self, now: Instant) -> (bool, bool) {
        let was = self.travelling.len();
        let Wordings { travelling, at } = self;
        travelling.retain(|_, ramp| !ramp.arrived(now));
        at.retain(|name, _| travelling.contains_key(name));
        for (name, ramp) in travelling.iter() {
            if let Some(leaving) = at.get_mut(name) {
                leaving.at = ramp.at(now);
            }
        }
        let going = !travelling.is_empty();
        (going, going || was > 0)
    }

    fn idle(&self) -> bool {
        self.travelling.is_empty()
    }
}

/// All animation state driven by the event loop's one shared travel timer.
#[derive(Default)]
pub(super) struct Travels {
    folds: Folds,
    module_folds: Folds,
    wordings: Wordings,
    scheduled: bool,
}

impl Travels {
    pub(super) fn turn_group(&mut self, name: String, to: f32, over: Duration) {
        self.folds.turn(name, to, over);
    }

    pub(super) fn settle_group(&mut self, name: &str) {
        self.folds.settle(name);
    }

    pub(super) fn turn_module(&mut self, name: String, to: f32, over: Duration) {
        self.module_folds.turn(name, to, over);
    }

    pub(super) fn settle_module(&mut self, name: &str) {
        self.module_folds.settle(name);
    }

    pub(super) fn start_wording(&mut self, name: String, from: usize, over: Duration) {
        self.wordings.start(name, from, over);
    }

    pub(super) fn settle_wording(&mut self, name: &str) {
        self.wordings.settle(name);
    }

    pub(super) fn folding(&self) -> &HashMap<String, f32> {
        &self.folds.at
    }

    pub(super) fn module_folding(&self) -> &HashMap<String, f32> {
        &self.module_folds.at
    }

    pub(super) fn switching(&self) -> &HashMap<String, Leaving> {
        &self.wordings.at
    }

    /// Move everything along, returning whether any remain and whether the frame changed.
    pub(super) fn step(&mut self, now: Instant) -> (bool, bool) {
        let (folding, folds_changed) = self.folds.step(now);
        let (module_folding, module_folds_changed) = self.module_folds.step(now);
        let (switching, wordings_changed) = self.wordings.step(now);
        let going = folding || module_folding || switching;
        self.scheduled = going;
        (
            going,
            folds_changed || module_folds_changed || wordings_changed,
        )
    }

    /// Claim the timer job once when new work appears.
    pub(super) fn claim(&mut self) -> bool {
        if self.scheduled || (self.folds.idle() && self.module_folds.idle() && self.wordings.idle())
        {
            return false;
        }
        self.scheduled = true;
        true
    }

    /// Give back a timer job which the event loop could not install.
    pub(super) fn release(&mut self) {
        self.scheduled = false;
    }
}

/// One bar that keeps out of sight until the pointer reaches its edge.
///
/// Only a deadline is kept: the event loop's hide timer exists while some bar has one, and
/// a bar that is shown and hovered, pinned or hidden costs nothing.
pub(super) struct Autohide {
    delay: Duration,
    pinned: bool,
    hovered: bool,
    hidden: bool,
    hide_at: Option<Instant>,
}

impl Autohide {
    /// A bar starts out of the way; the pointer or the signal is what brings it out.
    pub(super) fn new(delay: Duration) -> Autohide {
        Autohide {
            delay,
            pinned: false,
            hovered: false,
            hidden: true,
            hide_at: None,
        }
    }

    pub(super) fn hidden(&self) -> bool {
        self.hidden
    }

    /// When this bar hides next, if anything is counting down.
    pub(super) fn due(&self) -> Option<Instant> {
        self.hide_at
    }

    /// The pointer has arrived, returning whether that brought the bar out.
    pub(super) fn enter(&mut self) -> bool {
        self.hovered = true;
        self.hide_at = None;
        std::mem::replace(&mut self.hidden, false)
    }

    /// The pointer has gone. `held` is an open menu, which keeps the bar it hangs from.
    pub(super) fn leave(&mut self, now: Instant, held: bool) {
        self.hovered = false;
        self.wait(now, held);
    }

    /// Pin the bar in view or let it hide again, returning whether that brought it out.
    pub(super) fn pin(&mut self, pinned: bool, now: Instant, held: bool) -> bool {
        self.pinned = pinned;
        if pinned {
            self.hide_at = None;
            return std::mem::replace(&mut self.hidden, false);
        }
        self.wait(now, held);
        false
    }

    pub(super) fn pinned(&self) -> bool {
        self.pinned
    }

    /// Take a re-read config's delay without disturbing where the bar is in hiding.
    ///
    /// A countdown already running keeps the deadline it was given: moving it would either
    /// hide a bar early or hold out one that was about to go, and whatever the pointer does
    /// next starts its countdown at the new delay anyway.
    pub(super) fn set_delay(&mut self, delay: Duration) {
        self.delay = delay;
    }

    /// Start counting down again once whatever held the bar out has let go.
    pub(super) fn wait(&mut self, now: Instant, held: bool) {
        let staying = self.hidden || self.hovered || self.pinned || held;
        self.hide_at = (!staying).then(|| now + self.delay);
    }

    /// Hide the bar if its time has come, returning whether it did.
    pub(super) fn step(&mut self, now: Instant) -> bool {
        match self.hide_at {
            Some(at) if at <= now => {
                self.hide_at = None;
                self.hidden = true;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hiding_bar_comes_out_for_the_pointer_and_goes_after_its_delay() {
        let now = Instant::now();
        let delay = Duration::from_millis(500);
        let mut bar = Autohide::new(delay);
        assert!(bar.hidden());
        assert!(bar.enter());
        assert!(!bar.enter(), "already out");
        assert_eq!(bar.due(), None, "nothing counts down under the pointer");

        bar.leave(now, false);
        assert_eq!(bar.due(), Some(now + delay));
        assert!(!bar.step(now + delay / 2));
        assert!(!bar.hidden());
        assert!(bar.step(now + delay));
        assert!(bar.hidden());
        assert_eq!(bar.due(), None, "a hidden bar keeps no timer");
    }

    #[test]
    fn coming_back_before_the_delay_keeps_the_bar_out() {
        let now = Instant::now();
        let mut bar = Autohide::new(Duration::from_millis(500));
        bar.enter();
        bar.leave(now, false);
        bar.enter();
        assert_eq!(bar.due(), None);
        assert!(!bar.step(now + Duration::from_secs(1)));
        assert!(!bar.hidden());
    }

    #[test]
    fn an_open_menu_holds_the_bar_until_it_closes() {
        let now = Instant::now();
        let delay = Duration::from_millis(300);
        let mut bar = Autohide::new(delay);
        bar.enter();
        bar.leave(now, true);
        assert_eq!(bar.due(), None);
        bar.wait(now, false);
        assert_eq!(bar.due(), Some(now + delay));
    }

    #[test]
    fn a_pinned_bar_stays_out_until_it_is_let_go() {
        let now = Instant::now();
        let delay = Duration::from_millis(300);
        let mut bar = Autohide::new(delay);
        assert!(bar.pin(true, now, false), "pinning brings a hidden bar out");
        bar.enter();
        bar.leave(now, false);
        assert_eq!(bar.due(), None);

        assert!(!bar.pin(false, now, false));
        assert_eq!(bar.due(), Some(now + delay));
        assert!(bar.step(now + delay));
    }

    #[test]
    fn letting_go_under_the_pointer_waits_for_the_pointer_to_leave() {
        let now = Instant::now();
        let mut bar = Autohide::new(Duration::from_millis(300));
        bar.pin(true, now, false);
        bar.enter();
        bar.pin(false, now, false);
        assert_eq!(bar.due(), None);
    }

    #[test]
    fn a_fold_eases_between_its_two_ends_and_stops_at_them() {
        let started = Instant::now();
        let over = Duration::from_millis(200);
        let fold = Fold {
            from: 0.0,
            to: 1.0,
            started,
            over,
        };
        assert_eq!(fold.at(started), 0.0);
        assert_eq!(fold.at(started + over), 1.0);
        assert_eq!(fold.at(started + over * 3), 1.0);
        assert!(fold.arrived(started + over) && !fold.arrived(started + over / 2));

        let quarter = fold.at(started + over / 4);
        let half = fold.at(started + over / 2);
        assert!((half - 0.5).abs() < 0.001);
        assert!(quarter < 0.25, "a fold leaves slowly, got {quarter}");

        let back = Fold {
            from: 1.0,
            to: 0.0,
            started,
            over,
        };
        assert!((back.at(started + over / 4) - (1.0 - quarter)).abs() < 0.001);

        let turned = Fold {
            from: half,
            to: 0.0,
            started,
            over,
        };
        assert_eq!(turned.at(started), half);
        assert_eq!(turned.at(started + over), 0.0);
    }

    #[test]
    fn travels_run_a_timer_only_while_something_is_travelling() {
        let over = Duration::from_millis(200);
        let mut travels = Travels::default();

        assert!(!travels.claim());
        let (travelling, changed) = travels.step(Instant::now());
        assert!(!travelling && !changed);

        travels.folds.turn("a".to_string(), 1.0, over);
        assert_eq!(travels.folds.at.get("a").copied(), Some(0.0));
        assert!(travels.claim());
        assert!(!travels.claim());

        let started = travels.folds.travelling["a"].started;
        let (travelling, changed) = travels.step(started + over / 2);
        assert!(travelling && changed);
        assert!((travels.folds.at["a"] - 0.5).abs() < 0.001);

        let (travelling, changed) = travels.step(started + over);
        assert!(!travelling && changed);
        assert!(travels.folds.travelling.is_empty() && travels.folds.at.is_empty());
        assert!(!travels.scheduled);
        travels.folds.turn("a".to_string(), 0.0, over);
        assert!(travels.claim());
    }

    #[test]
    fn a_wording_and_a_fold_share_one_timer() {
        let over = Duration::from_millis(200);
        let mut travels = Travels::default();

        travels.wordings.start("m".to_string(), 0, over);
        assert!(travels.claim());
        travels.folds.turn("a".to_string(), 1.0, over);
        assert!(!travels.claim());

        let started = travels.wordings.travelling["m"].started;
        let (travelling, changed) = travels.step(started + over / 2);
        assert!(travelling && changed);
        assert!((travels.wordings.at["m"].at - 0.5).abs() < 0.001);
        assert_eq!(travels.wordings.at["m"].from, 0);

        let (travelling, _) = travels.step(started + over * 2);
        assert!(!travelling);
        assert!(travels.wordings.travelling.is_empty() && travels.wordings.at.is_empty());
    }

    #[test]
    fn a_wording_clicked_again_starts_over_from_its_new_origin() {
        let over = Duration::from_millis(200);
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

    #[test]
    fn a_released_timer_claim_can_be_taken_again() {
        let mut travels = Travels::default();
        travels
            .folds
            .turn("a".to_string(), 1.0, Duration::from_millis(200));
        assert!(travels.claim());
        travels.release();
        assert!(travels.claim());
    }

    #[test]
    fn turning_around_uses_only_the_remaining_distance() {
        let over = Duration::from_millis(200);
        let mut folds = Folds::default();
        folds.turn("a".to_string(), 1.0, over);
        let started = folds.travelling["a"].started;
        folds.step(started + over / 10);
        let caught = folds.at["a"];
        assert!(caught > 0.0 && caught < 0.1);

        folds.turn("a".to_string(), 0.0, over);
        let back = &folds.travelling["a"];
        assert_eq!(back.from, caught);
        assert_eq!(back.to, 0.0);
        assert!(back.over < over / 10);

        let mut folds = Folds::default();
        folds.turn("b".to_string(), 1.0, over);
        folds.turn("b".to_string(), 0.0, over);
        assert_eq!(folds.travelling["b"].from, folds.travelling["b"].to);
        let now = Instant::now();
        assert!(folds.travelling["b"].arrived(now));
        let (travelling, changed) = folds.step(now);
        assert!(!travelling && changed);
    }

    #[test]
    fn settling_one_fold_leaves_the_others_running() {
        let mut folds = Folds::default();
        let over = Duration::from_millis(200);
        folds.turn("a".to_string(), 1.0, over);
        folds.turn("b".to_string(), 1.0, over);
        folds.settle("a");
        assert!(!folds.travelling.contains_key("a") && !folds.at.contains_key("a"));
        assert!(folds.travelling.contains_key("b") && folds.at.contains_key("b"));
    }
}
