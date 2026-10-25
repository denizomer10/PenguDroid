//! Basic focus state for the per-process hosted audio service. The Linux audio
//! sink accepts this guest's playback. Cross-process arbitration, ducking and
//! listener callbacks still require a shared audio policy service.
use std::sync::atomic::{AtomicI32, Ordering};

pub struct AudioFocus(AtomicI32);
impl AudioFocus {
    pub const fn new() -> Self { Self(AtomicI32::new(0)) }
    pub fn request(&self, gain: i32) -> i32 {
        if !(1..=4).contains(&gain) { return 0; }
        self.0.store(gain, Ordering::Relaxed);
        1 // AudioManager.AUDIOFOCUS_REQUEST_GRANTED
    }
    pub fn current(&self) -> i32 { self.0.load(Ordering::Relaxed) }
    pub fn abandon(&self) -> i32 {
        self.0.store(0, Ordering::Relaxed);
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playback_focus_is_granted_then_released() {
        let focus = AudioFocus::new();
        assert_eq!(focus.current(), 0);
        assert_eq!(focus.request(1), 1);
        assert_eq!(focus.current(), 1);
        assert_eq!(focus.request(3), 1);
        assert_eq!(focus.current(), 3);
        assert_eq!(focus.abandon(), 1);
        assert_eq!(focus.current(), 0);
    }
    #[test]
    fn invalid_focus_gain_does_not_replace_the_existing_request() {
        let focus = AudioFocus::new();
        focus.request(1);
        assert_eq!(focus.request(0), 0);
        assert_eq!(focus.request(5), 0);
        assert_eq!(focus.current(), 1);
    }
}
