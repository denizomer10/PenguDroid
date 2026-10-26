//! Translate a per-app keymap binding into the touch events Android expects.
//!
//! The keymap itself lives in [`pengudroid_input::keymap`] and is authored in
//! the guest's pixel coordinates. [`crate::apphost`] feeds each physical key
//! event through [`steps`] and injects the result as `MotionEvent`s, so a bound
//! key never reaches the app as a key press: it becomes the tap, hold or swipe
//! the binding describes. Games that only understand a touchscreen can then be
//! played from the keyboard without a second input device.
//!
//! This module is deliberately pure: it returns events and delays instead of
//! touching JNI, which is what makes the gesture shape testable without ART.

use std::collections::BTreeMap;

use pengudroid_input::keymap::Action;

/// How long a synthetic tap stays down before it is released.
///
/// Android drops a press and release delivered in the same millisecond as a
/// hover, so the gap is load-bearing rather than cosmetic.
pub const TAP_HOLD_MS: u64 = 30;
/// Gap between interpolated points of a swipe.
pub const SWIPE_STEP_MS: u64 = 8;

/// One injected motion event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    Down { x: f32, y: f32 },
    Move { x: f32, y: f32 },
    Up { x: f32, y: f32 },
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

/// Touch points currently held by a mapped key.
///
/// A `Hold` binding must lift the point it pressed, and auto-repeat must not
/// re-run a tap or a swipe, so the key's state is tracked here until its
/// release arrives.
#[derive(Debug, Default)]
pub struct HeldPoints {
    points: BTreeMap<u16, (f32, f32)>,
}

impl HeldPoints {
    pub fn is_down(&self, code: u16) -> bool {
        self.points.contains_key(&code)
    }

    fn press(&mut self, code: u16, x: f32, y: f32) {
        self.points.insert(code, (x, y));
    }

    fn release(&mut self, code: u16) -> Option<(f32, f32)> {
        self.points.remove(&code)
    }
}

/// Steps for one key event.
///
/// Returns an empty list when the event changes nothing: a repeat press, or a
/// release of a gesture that already completed on its own (tap and swipe release
/// at the end of the press). `pressed` is the physical key state.
pub fn steps(action: &Action, code: u16, pressed: bool, held: &mut HeldPoints) -> Vec<Step> {
    match action {
        Action::Tap { x, y } => {
            if !pressed {
                // The gesture completed on press; the release only clears the
                // record so a later press can tap again.
                held.release(code);
                return Vec::new();
            }
            if held.is_down(code) {
                return Vec::new();
            }
            held.press(code, *x, *y);
            vec![
                Step::new(Event::Down { x: *x, y: *y }, 0),
                Step::new(Event::Up { x: *x, y: *y }, TAP_HOLD_MS),
            ]
        }
        Action::Hold { x, y } => {
            if pressed {
                if held.is_down(code) {
                    return Vec::new();
                }
                held.press(code, *x, *y);
                vec![Step::new(Event::Down { x: *x, y: *y }, 0)]
            } else {
                let Some((x, y)) = held.release(code) else {
                    return Vec::new();
                };
                vec![Step::new(Event::Up { x, y }, 0)]
            }
        }
        Action::Swipe { x0, y0, x1, y1, steps } => {
            if !pressed {
                held.release(code);
                return Vec::new();
            }
            if held.is_down(code) {
                return Vec::new();
            }
            held.press(code, *x0, *y0);
            let mut out = Vec::with_capacity(*steps as usize + 2);
            out.push(Step::new(Event::Down { x: *x0, y: *y0 }, 0));
            // `steps` counts the segments, so the intermediate points are
            // `i / steps`; the final point is the release itself.
            for i in 1..*steps {
                let t = i as f32 / *steps as f32;
                out.push(Step::new(
                    Event::Move {
                        x: lerp(*x0, *x1, t),
                        y: lerp(*y0, *y1, t),
                    },
                    SWIPE_STEP_MS,
                ));
            }
            out.push(Step::new(Event::Up { x: *x1, y: *y1 }, SWIPE_STEP_MS));
            out
        }
    }
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
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
                Step::new(Event::Down { x: 10.0, y: 20.0 }, 0),
                Step::new(Event::Up { x: 10.0, y: 20.0 }, TAP_HOLD_MS),
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
            vec![Step::new(Event::Down { x: 400.0, y: 900.0 }, 0)]
        );
        assert!(steps(&action, 17, true, &mut held).is_empty());
        assert_eq!(
            steps(&action, 17, false, &mut held),
            vec![Step::new(Event::Up { x: 400.0, y: 900.0 }, 0)]
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
        assert_eq!(out[0].event, Event::Down { x: 100.0, y: 200.0 });
        assert_eq!(out[1].event, Event::Move { x: 200.0, y: 300.0 });
        assert_eq!(out[2].event, Event::Move { x: 300.0, y: 400.0 });
        assert_eq!(out[3].event, Event::Move { x: 400.0, y: 500.0 });
        assert_eq!(out[4].event, Event::Up { x: 500.0, y: 600.0 });
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
        assert_eq!(out[1].event, Event::Up { x: 10.0, y: 0.0 });
    }
}
