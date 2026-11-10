//! Translate a per-app keymap binding into the touch events Android expects.
//!
//! The keymap itself lives in [`pengudroid_input::keymap`] and is authored in
//! the guest's pixel coordinates. [`crate::apphost`] feeds each physical key
//! event through [`steps`] and injects the result as `MotionEvent`s, so a bound
//! key never reaches the app as a key press: it becomes the tap, hold, swipe,
//! joystick drag or camera drag the binding describes. Games that only understand a
//! touchscreen can then be played from the keyboard without a second input
//! device.
//!
//! Every gesture owns its own *pointer id*. A game is not playable with one
//! finger: walking while turning the camera while pressing a skill is three
//! simultaneous touches. [`steps`] says which finger does what, and
//! [`TouchState`] folds those per-finger events into the multi-pointer frames
//! `MotionEvent` carries (`ACTION_POINTER_DOWN` with every live pointer listed,
//! and so on).
//!
//! This module is deliberately pure: it returns events and delays instead of
//! touching JNI, which is what makes the gesture shape testable without ART.

use std::collections::BTreeMap;

use pengudroid_input::keymap::{Action, Dir};

/// How long a synthetic tap stays down before it is released.
///
/// Android drops a press and release delivered in the same millisecond as a
/// hover, so the gap is load-bearing rather than cosmetic.
pub const TAP_HOLD_MS: u64 = 30;
/// Gap between interpolated points of a swipe.
pub const SWIPE_STEP_MS: u64 = 8;
/// Gap between a joystick finger landing on the centre and its first drag.
///
/// A floating movement pad anchors itself where the finger lands; the drag has
/// to arrive as a later event or the pad sees a touch that began off-centre.
pub const STICK_ENGAGE_MS: u64 = 16;

/// The pointer id of the real mouse/touch pointer. Mapped gestures never use it.
pub const MOUSE_POINTER: i32 = 0;
/// Mapped gestures take ids from `1..=MAX_MAPPED_POINTERS`.
///
/// `MotionEvent` supports 16 pointers; ten fingers is already more than any
/// profile presses at once, and running out drops the gesture instead of
/// corrupting another finger's stream.
pub const MAX_MAPPED_POINTERS: i32 = 10;

/// One injected motion event, for the finger `id`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    Down { id: i32, x: f32, y: f32 },
    Move { id: i32, x: f32, y: f32 },
    Up { id: i32, x: f32, y: f32 },
}

/// An event and how long to wait before injecting it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub event: Event,
    pub delay_ms: u64,
}

impl Step {
    fn new(event: Event, delay_ms: u64) -> Step {
        Step { event, delay_ms }
    }
}

/// What a key that is currently down is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Held {
    /// A `hold`: the finger stays at this point until the key is released.
    Point { id: i32, x: f32, y: f32 },
    /// A tap or swipe that already finished; kept so auto-repeat is ignored.
    Done,
    /// One direction of the joystick `stick`.
    Stick { stick: StickKey, dir: Dir },
}

/// Identity of a joystick: keys with the same centre and radius share a finger.
type StickKey = (u32, u32, u32);

fn stick_key(cx: f32, cy: f32, radius: f32) -> StickKey {
    (cx.to_bits(), cy.to_bits(), radius.to_bits())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct StickState {
    id: i32,
    /// Where the finger currently is.
    at: (f32, f32),
}

/// Mouse-look while it is switched on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LookState {
    cx: f32,
    cy: f32,
    radius: f32,
    sensitivity: f32,
    /// The camera finger and where it is, while it is on the screen.
    finger: Option<(i32, (f32, f32))>,
}

/// State of every mapped key that is currently held.
///
/// A `Hold` binding must lift the point it pressed, a joystick must know which
/// of its directions are still down, and auto-repeat must not re-run a tap or a
/// swipe, so the key's state is tracked here until its release arrives.
#[derive(Debug, Default)]
pub struct HeldPoints {
    keys: BTreeMap<u16, Held>,
    sticks: BTreeMap<StickKey, StickState>,
    /// `Some` while mouse-look is on (it is a toggle, not a held key).
    look: Option<LookState>,
}

impl HeldPoints {
    pub fn is_down(&self, code: u16) -> bool {
        self.keys.contains_key(&code)
    }

    /// The lowest pointer id no held gesture owns.
    fn free_id(&self) -> Option<i32> {
        (1..=MAX_MAPPED_POINTERS).find(|id| {
            !self.sticks.values().any(|s| s.id == *id)
                && self.look.and_then(|l| l.finger).map(|(held, _)| held) != Some(*id)
                && !self
                    .keys
                    .values()
                    .any(|h| matches!(h, Held::Point { id: held, .. } if held == id))
        })
    }

    /// Where the joystick's finger belongs for the directions now held.
    fn stick_target(&self, stick: StickKey, cx: f32, cy: f32, radius: f32) -> (f32, f32) {
        let (mut dx, mut dy) = (0.0f32, 0.0f32);
        for held in self.keys.values() {
            if let Held::Stick { stick: s, dir } = held {
                if *s == stick {
                    let (vx, vy) = dir.vector();
                    dx += vx;
                    dy += vy;
                }
            }
        }
        // Opposite keys cancel; a diagonal is normalised so it does not push
        // the finger past the rim (which a pad would read as the same speed
        // anyway, but a rim it does not know about could be off-screen).
        let len = (dx * dx + dy * dy).sqrt();
        if len < f32::EPSILON {
            return (cx, cy);
        }
        (cx + dx / len * radius, cy + dy / len * radius)
    }

    fn stick_has_keys(&self, stick: StickKey) -> bool {
        self.keys
            .values()
            .any(|h| matches!(h, Held::Stick { stick: s, .. } if *s == stick))
    }

    /// Whether mouse-look is on. The caller keeps the pointer lock in step
    /// with this.
    pub fn look_active(&self) -> bool {
        self.look.is_some()
    }

    /// Whether the camera finger is currently on the screen.
    pub fn look_finger_down(&self) -> bool {
        self.look.is_some_and(|l| l.finger.is_some())
    }

    /// Relative mouse motion while mouse-look is on: drag the camera finger.
    ///
    /// The finger lands on the centre, follows the mouse, and when it would
    /// leave the allowed circle it is lifted and put back on the centre first.
    /// A touch game turns the camera by how far a finger *moves*, so the
    /// re-centring is invisible to it apart from a new touch beginning.
    pub fn look_motion(&mut self, dx: f32, dy: f32) -> Vec<Step> {
        let Some(mut look) = self.look else {
            return Vec::new();
        };
        if !(dx.is_finite() && dy.is_finite()) || (dx == 0.0 && dy == 0.0) {
            return Vec::new();
        }
        let centre = (look.cx, look.cy);
        // A single motion larger than the circle still has to end inside it.
        let limit = |x: f32, y: f32| {
            let (ox, oy) = (x - centre.0, y - centre.1);
            let len = (ox * ox + oy * oy).sqrt();
            if len > look.radius {
                (centre.0 + ox / len * look.radius, centre.1 + oy / len * look.radius)
            } else {
                (x, y)
            }
        };
        let (mx, my) = (dx * look.sensitivity, dy * look.sensitivity);
        let mut out = Vec::new();
        let (id, mut at) = match look.finger {
            Some(finger) => finger,
            None => {
                let Some(id) = self.free_id() else { return Vec::new() };
                out.push(Step::new(Event::Down { id, x: centre.0, y: centre.1 }, 0));
                (id, centre)
            }
        };
        let (tx, ty) = (at.0 + mx, at.1 + my);
        let outside = {
            let (ox, oy) = (tx - centre.0, ty - centre.1);
            (ox * ox + oy * oy).sqrt() > look.radius
        };
        if outside && at != centre {
            out.push(Step::new(Event::Up { id, x: at.0, y: at.1 }, 0));
            out.push(Step::new(Event::Down { id, x: centre.0, y: centre.1 }, 0));
            at = centre;
        }
        let target = limit(at.0 + mx, at.1 + my);
        out.push(Step::new(Event::Move { id, x: target.0, y: target.1 }, 0));
        look.finger = Some((id, target));
        self.look = Some(look);
        out
    }

    /// The mouse stopped: lift the camera finger so the next motion starts
    /// from the centre again.
    pub fn look_idle(&mut self) -> Vec<Step> {
        let Some(look) = self.look.as_mut() else {
            return Vec::new();
        };
        match look.finger.take() {
            Some((id, (x, y))) => vec![Step::new(Event::Up { id, x, y }, 0)],
            None => Vec::new(),
        }
    }

    /// Switch mouse-look off (the pointer could not be locked, for example).
    pub fn look_cancel(&mut self) -> Vec<Step> {
        let out = self.look_idle();
        self.look = None;
        out
    }

    /// Lift every finger a mapped key still owns.
    ///
    /// The daemon already synthesises key releases when the window loses
    /// keyboard focus, so the pump does not need this; the tests use it to
    /// prove no sequence of keys can leave a finger behind.
    #[cfg(test)]
    pub fn release_all(&mut self) -> Vec<Step> {
        let mut out = Vec::new();
        for held in self.keys.values() {
            if let Held::Point { id, x, y } = held {
                out.push(Step::new(Event::Up { id: *id, x: *x, y: *y }, 0));
            }
        }
        for stick in self.sticks.values() {
            out.push(Step::new(Event::Up { id: stick.id, x: stick.at.0, y: stick.at.1 }, 0));
        }
        self.keys.clear();
        self.sticks.clear();
        out.extend(self.look_cancel());
        out
    }
}

/// Steps for one key event.
///
/// Returns an empty list when the event changes nothing: a repeat press, or a
/// release of a gesture that already completed on its own (tap and swipe release
/// at the end of the press). `pressed` is the physical key state.
pub fn steps(action: &Action, code: u16, pressed: bool, held: &mut HeldPoints) -> Vec<Step> {
    if pressed && held.is_down(code) {
        // Auto-repeat, or a second keyboard sending the same key.
        return Vec::new();
    }
    if !pressed {
        return release(action, code, held);
    }
    match action {
        Action::Tap { x, y } => {
            let Some(id) = held.free_id() else { return Vec::new() };
            held.keys.insert(code, Held::Done);
            vec![
                Step::new(Event::Down { id, x: *x, y: *y }, 0),
                Step::new(Event::Up { id, x: *x, y: *y }, TAP_HOLD_MS),
            ]
        }
        Action::Hold { x, y } => {
            let Some(id) = held.free_id() else { return Vec::new() };
            held.keys.insert(code, Held::Point { id, x: *x, y: *y });
            vec![Step::new(Event::Down { id, x: *x, y: *y }, 0)]
        }
        Action::Swipe { x0, y0, x1, y1, steps } => {
            let Some(id) = held.free_id() else { return Vec::new() };
            held.keys.insert(code, Held::Done);
            let mut out = Vec::with_capacity(*steps as usize + 2);
            out.push(Step::new(Event::Down { id, x: *x0, y: *y0 }, 0));
            // `steps` counts the segments, so the intermediate points are
            // `i / steps`; the final point is the release itself.
            for i in 1..*steps {
                let t = i as f32 / *steps as f32;
                out.push(Step::new(
                    Event::Move {
                        id,
                        x: lerp(*x0, *x1, t),
                        y: lerp(*y0, *y1, t),
                    },
                    SWIPE_STEP_MS,
                ));
            }
            out.push(Step::new(Event::Up { id, x: *x1, y: *y1 }, SWIPE_STEP_MS));
            out
        }
        Action::Look { cx, cy, radius, sensitivity } => {
            // A toggle: the press flips it, the release does nothing.
            held.keys.insert(code, Held::Done);
            if held.look.is_some() {
                return held.look_cancel();
            }
            held.look = Some(LookState {
                cx: *cx,
                cy: *cy,
                radius: *radius,
                sensitivity: *sensitivity,
                finger: None,
            });
            Vec::new()
        }
        Action::Stick { cx, cy, radius, dir } => {
            let stick = stick_key(*cx, *cy, *radius);
            let mut out = Vec::new();
            let (id, engaged) = match held.sticks.get(&stick) {
                Some(state) => (state.id, true),
                None => {
                    let Some(id) = held.free_id() else { return Vec::new() };
                    (id, false)
                }
            };
            held.keys.insert(code, Held::Stick { stick, dir: *dir });
            let target = held.stick_target(stick, *cx, *cy, *radius);
            if !engaged {
                out.push(Step::new(Event::Down { id, x: *cx, y: *cy }, 0));
            }
            out.push(Step::new(
                Event::Move { id, x: target.0, y: target.1 },
                if engaged { 0 } else { STICK_ENGAGE_MS },
            ));
            held.sticks.insert(stick, StickState { id, at: target });
            out
        }
    }
}

fn release(action: &Action, code: u16, held: &mut HeldPoints) -> Vec<Step> {
    // The record decides what the release does, not the binding: the profile
    // may have been edited while the key was down.
    match held.keys.remove(&code) {
        None | Some(Held::Done) => Vec::new(),
        Some(Held::Point { id, x, y }) => vec![Step::new(Event::Up { id, x, y }, 0)],
        Some(Held::Stick { stick, .. }) => {
            let Some(state) = held.sticks.get(&stick).copied() else {
                return Vec::new();
            };
            if !held.stick_has_keys(stick) {
                held.sticks.remove(&stick);
                return vec![Step::new(
                    Event::Up { id: state.id, x: state.at.0, y: state.at.1 },
                    0,
                )];
            }
            let (cx, cy, radius) = match action {
                Action::Stick { cx, cy, radius, .. } => (*cx, *cy, *radius),
                _ => (
                    f32::from_bits(stick.0),
                    f32::from_bits(stick.1),
                    f32::from_bits(stick.2),
                ),
            };
            let target = held.stick_target(stick, cx, cy, radius);
            held.sticks.insert(stick, StickState { id: state.id, at: target });
            vec![Step::new(Event::Move { id: state.id, x: target.0, y: target.1 }, 0)]
        }
    }
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// `MotionEvent.ACTION_*` values used by [`TouchState`].
pub const ACTION_DOWN: i32 = 0;
pub const ACTION_UP: i32 = 1;
pub const ACTION_MOVE: i32 = 2;
pub const ACTION_POINTER_DOWN: i32 = 5;
pub const ACTION_POINTER_UP: i32 = 6;
/// `MotionEvent.ACTION_POINTER_INDEX_SHIFT`.
pub const ACTION_POINTER_INDEX_SHIFT: i32 = 8;

/// One finger in a [`Frame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pointer {
    pub id: i32,
    pub x: f32,
    pub y: f32,
}

/// One `MotionEvent`: the action and every pointer that is part of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// `ACTION_*`, with the pointer index in the high bits for
    /// `ACTION_POINTER_DOWN`/`ACTION_POINTER_UP`.
    pub action: i32,
    /// Every live pointer, ordered by id. For an `UP`/`POINTER_UP` this still
    /// includes the pointer that is leaving, as Android requires.
    pub pointers: Vec<Pointer>,
    /// True for the `ACTION_DOWN` that begins a gesture: the caller takes a new
    /// `downTime` here and reuses it until the last pointer lifts.
    pub starts_gesture: bool,
}

/// Folds per-finger events into the multi-pointer stream Android expects.
///
/// Android has one event stream per window, not per finger: the first finger
/// is `ACTION_DOWN`, later ones are `ACTION_POINTER_DOWN` carrying the index of
/// the new pointer, a move lists every finger, and only the last one to leave
/// produces `ACTION_UP`. Views (and `InputEventConsistencyVerifier`) reject
/// anything else, so this is the single place that sequencing lives.
#[derive(Debug, Default)]
pub struct TouchState {
    active: Vec<Pointer>,
}

impl TouchState {
    pub fn is_idle(&self) -> bool {
        self.active.is_empty()
    }

    #[cfg(test)]
    pub fn pointer_count(&self) -> usize {
        self.active.len()
    }

    fn index_of(&self, id: i32) -> Option<usize> {
        self.active.iter().position(|p| p.id == id)
    }

    /// Apply one finger event; `None` when it is not a valid transition (a move
    /// or release for a finger that is not down).
    pub fn apply(&mut self, event: Event) -> Option<Frame> {
        match event {
            Event::Down { id, x, y } => {
                if let Some(index) = self.index_of(id) {
                    // Already down: a second Down would be rejected by the
                    // framework, so it degrades to a move.
                    self.active[index] = Pointer { id, x, y };
                    return Some(self.frame(ACTION_MOVE, false));
                }
                let index = self.active.partition_point(|p| p.id < id);
                self.active.insert(index, Pointer { id, x, y });
                if self.active.len() == 1 {
                    Some(self.frame(ACTION_DOWN, true))
                } else {
                    Some(self.frame(
                        ACTION_POINTER_DOWN | ((index as i32) << ACTION_POINTER_INDEX_SHIFT),
                        false,
                    ))
                }
            }
            Event::Move { id, x, y } => {
                let index = self.index_of(id)?;
                self.active[index] = Pointer { id, x, y };
                Some(self.frame(ACTION_MOVE, false))
            }
            Event::Up { id, x, y } => {
                let index = self.index_of(id)?;
                self.active[index] = Pointer { id, x, y };
                let frame = if self.active.len() == 1 {
                    self.frame(ACTION_UP, false)
                } else {
                    self.frame(
                        ACTION_POINTER_UP | ((index as i32) << ACTION_POINTER_INDEX_SHIFT),
                        false,
                    )
                };
                self.active.remove(index);
                Some(frame)
            }
        }
    }

    fn frame(&self, action: i32, starts_gesture: bool) -> Frame {
        Frame { action, pointers: self.active.clone(), starts_gesture }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_presses_and_releases_once() {
        let mut held = HeldPoints::default();
        let action = Action::Tap { x: 10.0, y: 20.0 };
        assert_eq!(
            steps(&action, 57, true, &mut held),
            vec![
                Step::new(Event::Down { id: 1, x: 10.0, y: 20.0 }, 0),
                Step::new(Event::Up { id: 1, x: 10.0, y: 20.0 }, TAP_HOLD_MS),
            ]
        );
        // Auto-repeat while the key is held must not stack taps.
        assert!(steps(&action, 57, true, &mut held).is_empty());
        // The release clears the record without emitting a second up.
        assert!(steps(&action, 57, false, &mut held).is_empty());
        assert!(!held.is_down(57));
    }

    #[test]
    fn hold_tracks_the_point_between_press_and_release() {
        let mut held = HeldPoints::default();
        let action = Action::Hold { x: 400.0, y: 900.0 };
        assert_eq!(
            steps(&action, 17, true, &mut held),
            vec![Step::new(Event::Down { id: 1, x: 400.0, y: 900.0 }, 0)]
        );
        assert!(steps(&action, 17, true, &mut held).is_empty());
        assert_eq!(
            steps(&action, 17, false, &mut held),
            vec![Step::new(Event::Up { id: 1, x: 400.0, y: 900.0 }, 0)]
        );
        // A stray release must not lift somebody else's finger.
        assert!(steps(&action, 17, false, &mut held).is_empty());
    }

    #[test]
    fn swipe_interpolates_and_ends_at_the_target() {
        let mut held = HeldPoints::default();
        let action = Action::Swipe {
            x0: 100.0,
            y0: 200.0,
            x1: 500.0,
            y1: 600.0,
            steps: 4,
        };
        let out = steps(&action, 42, true, &mut held);
        assert_eq!(out.len(), 5, "down + three intermediate points + up");
        assert_eq!(out[0].event, Event::Down { id: 1, x: 100.0, y: 200.0 });
        assert_eq!(out[1].event, Event::Move { id: 1, x: 200.0, y: 300.0 });
        assert_eq!(out[2].event, Event::Move { id: 1, x: 300.0, y: 400.0 });
        assert_eq!(out[3].event, Event::Move { id: 1, x: 400.0, y: 500.0 });
        assert_eq!(out[4].event, Event::Up { id: 1, x: 500.0, y: 600.0 });
        assert!(out[1..].iter().all(|s| s.delay_ms == SWIPE_STEP_MS));
        assert!(steps(&action, 42, true, &mut held).is_empty(), "no repeat");
        assert!(steps(&action, 42, false, &mut held).is_empty());
        assert!(!held.is_down(42));
    }

    #[test]
    fn single_segment_swipe_is_down_then_up() {
        let mut held = HeldPoints::default();
        let action = Action::Swipe { x0: 0.0, y0: 0.0, x1: 10.0, y1: 0.0, steps: 1 };
        let out = steps(&action, 1, true, &mut held);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].event, Event::Up { id: 1, x: 10.0, y: 0.0 });
    }

    fn stick(dir: Dir) -> Action {
        Action::Stick { cx: 200.0, cy: 500.0, radius: 100.0, dir }
    }

    #[test]
    fn stick_lands_on_the_centre_then_drags() {
        let mut held = HeldPoints::default();
        assert_eq!(
            steps(&stick(Dir::Up), 17, true, &mut held),
            vec![
                Step::new(Event::Down { id: 1, x: 200.0, y: 500.0 }, 0),
                Step::new(Event::Move { id: 1, x: 200.0, y: 400.0 }, STICK_ENGAGE_MS),
            ]
        );
        assert!(steps(&stick(Dir::Up), 17, true, &mut held).is_empty(), "no repeat");
        assert_eq!(
            steps(&stick(Dir::Up), 17, false, &mut held),
            vec![Step::new(Event::Up { id: 1, x: 200.0, y: 400.0 }, 0)]
        );
        assert!(steps(&stick(Dir::Up), 17, false, &mut held).is_empty());
    }

    #[test]
    fn stick_directions_share_one_finger_and_combine() {
        let mut held = HeldPoints::default();
        steps(&stick(Dir::Up), 17, true, &mut held);
        // W + D: the same finger moves to the normalised diagonal.
        let out = steps(&stick(Dir::Right), 32, true, &mut held);
        let diag = 100.0 * std::f32::consts::FRAC_1_SQRT_2;
        assert_eq!(out.len(), 1);
        let Event::Move { id, x, y } = out[0].event else { panic!("{out:?}") };
        assert_eq!(id, 1);
        assert!((x - (200.0 + diag)).abs() < 0.01 && (y - (500.0 - diag)).abs() < 0.01);
        // Releasing W leaves the finger down, now pointing right.
        assert_eq!(
            steps(&stick(Dir::Up), 17, false, &mut held),
            vec![Step::new(Event::Move { id: 1, x: 300.0, y: 500.0 }, 0)]
        );
        // The last key lifts it.
        assert_eq!(
            steps(&stick(Dir::Right), 32, false, &mut held),
            vec![Step::new(Event::Up { id: 1, x: 300.0, y: 500.0 }, 0)]
        );
    }

    #[test]
    fn opposite_stick_keys_rest_on_the_centre() {
        let mut held = HeldPoints::default();
        steps(&stick(Dir::Left), 30, true, &mut held);
        assert_eq!(
            steps(&stick(Dir::Right), 32, true, &mut held),
            vec![Step::new(Event::Move { id: 1, x: 200.0, y: 500.0 }, 0)]
        );
    }

    #[test]
    fn concurrent_gestures_get_distinct_fingers() {
        let mut held = HeldPoints::default();
        steps(&stick(Dir::Up), 17, true, &mut held);
        let hold = Action::Hold { x: 900.0, y: 600.0 };
        assert_eq!(
            steps(&hold, 42, true, &mut held),
            vec![Step::new(Event::Down { id: 2, x: 900.0, y: 600.0 }, 0)]
        );
        // A tap while both are down takes a third finger and gives it back.
        let tap = Action::Tap { x: 1.0, y: 2.0 };
        let out = steps(&tap, 57, true, &mut held);
        assert_eq!(out[0].event, Event::Down { id: 3, x: 1.0, y: 2.0 });
        steps(&tap, 57, false, &mut held);
        // Releasing the stick frees finger 1 for the next gesture.
        steps(&stick(Dir::Up), 17, false, &mut held);
        let out = steps(&tap, 57, true, &mut held);
        assert_eq!(out[0].event, Event::Down { id: 1, x: 1.0, y: 2.0 });
    }

    #[test]
    fn never_uses_the_mouse_pointer_and_stops_when_out_of_fingers() {
        let mut held = HeldPoints::default();
        for code in 0..MAX_MAPPED_POINTERS as u16 {
            let out = steps(&Action::Hold { x: 0.0, y: 0.0 }, 100 + code, true, &mut held);
            let Event::Down { id, .. } = out[0].event else { panic!() };
            assert_ne!(id, MOUSE_POINTER);
        }
        assert!(steps(&Action::Hold { x: 0.0, y: 0.0 }, 99, true, &mut held).is_empty());
        assert!(!held.is_down(99));
    }

    #[test]
    fn release_all_lifts_every_finger() {
        let mut held = HeldPoints::default();
        steps(&stick(Dir::Up), 17, true, &mut held);
        steps(&Action::Hold { x: 9.0, y: 8.0 }, 42, true, &mut held);
        let out = held.release_all();
        assert_eq!(out.len(), 2);
        assert!(out.contains(&Step::new(Event::Up { id: 2, x: 9.0, y: 8.0 }, 0)));
        assert!(out.contains(&Step::new(Event::Up { id: 1, x: 200.0, y: 400.0 }, 0)));
        assert!(!held.is_down(17) && !held.is_down(42));
        assert!(held.release_all().is_empty());
    }

    fn look() -> Action {
        Action::Look { cx: 800.0, cy: 300.0, radius: 100.0, sensitivity: 2.0 }
    }

    #[test]
    fn look_is_a_toggle_and_moves_nothing_until_the_mouse_does() {
        let mut held = HeldPoints::default();
        assert!(held.look_motion(5.0, 5.0).is_empty(), "off: motion is ignored");
        assert!(steps(&look(), 56, true, &mut held).is_empty());
        assert!(held.look_active() && !held.look_finger_down());
        // Auto-repeat and the release must not flip it back.
        assert!(steps(&look(), 56, true, &mut held).is_empty());
        assert!(steps(&look(), 56, false, &mut held).is_empty());
        assert!(held.look_active());
        // The second press switches it off.
        steps(&look(), 56, true, &mut held);
        assert!(!held.look_active());
    }

    #[test]
    fn look_drags_from_the_centre_scaled_by_sensitivity() {
        let mut held = HeldPoints::default();
        steps(&look(), 56, true, &mut held);
        assert_eq!(
            held.look_motion(10.0, -5.0),
            vec![
                Step::new(Event::Down { id: 1, x: 800.0, y: 300.0 }, 0),
                Step::new(Event::Move { id: 1, x: 820.0, y: 290.0 }, 0),
            ]
        );
        assert_eq!(
            held.look_motion(10.0, 0.0),
            vec![Step::new(Event::Move { id: 1, x: 840.0, y: 290.0 }, 0)]
        );
        assert!(held.look_motion(0.0, 0.0).is_empty());
        assert!(held.look_motion(f32::NAN, 1.0).is_empty());
        // Idle lifts the finger where it is; the next motion starts over.
        assert_eq!(
            held.look_idle(),
            vec![Step::new(Event::Up { id: 1, x: 840.0, y: 290.0 }, 0)]
        );
        assert!(held.look_idle().is_empty());
        assert_eq!(held.look_motion(1.0, 0.0)[0].event, Event::Down { id: 1, x: 800.0, y: 300.0 });
    }

    #[test]
    fn look_recentres_instead_of_leaving_its_circle() {
        let mut held = HeldPoints::default();
        steps(&look(), 56, true, &mut held);
        held.look_motion(40.0, 0.0); // finger at 880
        // Another 40 would reach 960, outside the 100 px circle: lift, land on
        // the centre, and continue the same motion from there.
        assert_eq!(
            held.look_motion(40.0, 0.0),
            vec![
                Step::new(Event::Up { id: 1, x: 880.0, y: 300.0 }, 0),
                Step::new(Event::Down { id: 1, x: 800.0, y: 300.0 }, 0),
                Step::new(Event::Move { id: 1, x: 880.0, y: 300.0 }, 0),
            ]
        );
        // A flick far larger than the circle is clamped to its rim.
        held.look_idle();
        let out = held.look_motion(0.0, 1000.0);
        assert_eq!(out.last().unwrap().event, Event::Move { id: 1, x: 800.0, y: 400.0 });
    }

    #[test]
    fn look_finger_coexists_with_the_stick_and_cancel_lifts_it() {
        let mut held = HeldPoints::default();
        steps(&stick(Dir::Up), 17, true, &mut held);
        steps(&look(), 56, true, &mut held);
        let out = held.look_motion(1.0, 1.0);
        assert_eq!(out[0].event, Event::Down { id: 2, x: 800.0, y: 300.0 });
        // A hold pressed now must not reuse the camera finger.
        let hold = steps(&Action::Hold { x: 1.0, y: 1.0 }, 42, true, &mut held);
        assert_eq!(hold[0].event, Event::Down { id: 3, x: 1.0, y: 1.0 });
        assert_eq!(
            held.look_cancel(),
            vec![Step::new(Event::Up { id: 2, x: 802.0, y: 302.0 }, 0)]
        );
        assert!(!held.look_active());
    }

    /// Mouse-look mixed with keys still yields a legal Android stream.
    #[test]
    fn look_and_keys_form_a_legal_stream() {
        let mut held = HeldPoints::default();
        let mut touch = TouchState::default();
        let mut apply = |events: Vec<Step>, touch: &mut TouchState| {
            for step in events {
                touch.apply(step.event).expect("legal transition");
            }
        };
        apply(steps(&look(), 56, true, &mut held), &mut touch);
        let mut seed = 0x9e37_79b9u32;
        for round in 0..3000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let dx = (seed % 61) as f32 - 30.0;
            let dy = ((seed >> 8) % 61) as f32 - 30.0;
            match seed % 5 {
                0 => apply(steps(&stick(Dir::Left), 30, round % 2 == 0, &mut held), &mut touch),
                1 => apply(held.look_idle(), &mut touch),
                _ => apply(held.look_motion(dx, dy), &mut touch),
            }
        }
        apply(held.release_all(), &mut touch);
        assert!(touch.is_idle());
    }

    fn p(id: i32, x: f32, y: f32) -> Pointer {
        Pointer { id, x, y }
    }

    #[test]
    fn touch_state_sequences_two_fingers_like_android() {
        let mut touch = TouchState::default();
        // The joystick finger (id 1) lands first, then the mouse (id 0).
        let f = touch.apply(Event::Down { id: 1, x: 10.0, y: 10.0 }).unwrap();
        assert_eq!((f.action, f.starts_gesture), (ACTION_DOWN, true));
        assert_eq!(f.pointers, vec![p(1, 10.0, 10.0)]);

        let f = touch.apply(Event::Down { id: 0, x: 50.0, y: 50.0 }).unwrap();
        // Pointers are ordered by id, so the mouse is index 0.
        assert_eq!(f.action, ACTION_POINTER_DOWN);
        assert!(!f.starts_gesture);
        assert_eq!(f.pointers, vec![p(0, 50.0, 50.0), p(1, 10.0, 10.0)]);

        let f = touch.apply(Event::Move { id: 1, x: 20.0, y: 10.0 }).unwrap();
        assert_eq!(f.action, ACTION_MOVE);
        assert_eq!(f.pointers, vec![p(0, 50.0, 50.0), p(1, 20.0, 10.0)]);

        // Lifting the joystick: index 1, still listed in the frame.
        let f = touch.apply(Event::Up { id: 1, x: 20.0, y: 10.0 }).unwrap();
        assert_eq!(f.action, ACTION_POINTER_UP | (1 << ACTION_POINTER_INDEX_SHIFT));
        assert_eq!(f.pointers.len(), 2);
        assert_eq!(touch.pointer_count(), 1);

        let f = touch.apply(Event::Up { id: 0, x: 50.0, y: 50.0 }).unwrap();
        assert_eq!(f.action, ACTION_UP);
        assert_eq!(f.pointers, vec![p(0, 50.0, 50.0)]);
        assert!(touch.is_idle());

        // The next touch starts a new gesture.
        let f = touch.apply(Event::Down { id: 3, x: 1.0, y: 1.0 }).unwrap();
        assert!(f.starts_gesture);
    }

    #[test]
    fn touch_state_rejects_events_for_fingers_that_are_not_down() {
        let mut touch = TouchState::default();
        assert!(touch.apply(Event::Move { id: 1, x: 0.0, y: 0.0 }).is_none());
        assert!(touch.apply(Event::Up { id: 1, x: 0.0, y: 0.0 }).is_none());
        touch.apply(Event::Down { id: 1, x: 0.0, y: 0.0 }).unwrap();
        // A duplicate Down must not open a second pointer.
        let f = touch.apply(Event::Down { id: 1, x: 5.0, y: 0.0 }).unwrap();
        assert_eq!(f.action, ACTION_MOVE);
        assert_eq!(touch.pointer_count(), 1);
    }

    /// Whatever keys are mashed, the stream handed to Android stays legal:
    /// exactly one DOWN per gesture, indices in range, and it ends idle.
    #[test]
    fn mapped_gestures_always_form_a_legal_stream() {
        let bindings: [(u16, Action); 6] = [
            (17, stick(Dir::Up)),
            (30, stick(Dir::Left)),
            (31, stick(Dir::Down)),
            (32, stick(Dir::Right)),
            (42, Action::Hold { x: 900.0, y: 600.0 }),
            (57, Action::Tap { x: 800.0, y: 650.0 }),
        ];
        let mut held = HeldPoints::default();
        let mut touch = TouchState::default();
        let mut down = [false; 6];
        // A fixed pseudo-random walk: deterministic, but long enough to hit
        // every interleaving of six keys.
        let mut seed = 0x2545_f491u32;
        let mut check = |events: Vec<Step>, touch: &mut TouchState| {
            for step in events {
                let before = touch.pointer_count();
                let frame = touch.apply(step.event).expect("legal transition");
                let masked = frame.action & 0xff;
                let index = (frame.action >> ACTION_POINTER_INDEX_SHIFT) as usize;
                assert!(index < frame.pointers.len());
                assert_eq!(frame.starts_gesture, masked == ACTION_DOWN);
                assert_eq!(masked == ACTION_DOWN, before == 0 && touch.pointer_count() == 1);
                assert!(frame.pointers.windows(2).all(|w| w[0].id < w[1].id));
            }
        };
        for _ in 0..4000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let key = (seed % 6) as usize;
            down[key] = !down[key];
            let (code, action) = &bindings[key];
            check(steps(action, *code, down[key], &mut held), &mut touch);
        }
        check(held.release_all(), &mut touch);
        assert!(touch.is_idle());
    }
}
