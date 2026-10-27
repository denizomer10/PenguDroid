//! Basic focus state for the per-process hosted audio service. The Linux audio
//! sink accepts this guest's playback. Cross-process arbitration, ducking and
//! listener callbacks still require a shared audio policy service.
use std::sync::atomic::{AtomicI32, Ordering};

/// Semantic operations are independent of generated AIDL transaction numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation { Request, Abandon, Unregister, Current }

#[derive(Clone, Copy, Debug)]
pub struct Transactions([u32; 4]);
impl Transactions {
    /// Validate codes read from the loaded API-37 IAudioService Stub, in
    /// request/abandon/unregister/current order. There is no default table.
    pub fn new(codes: [u32; 4]) -> Option<Self> {
        if codes.iter().any(|&c| c == 0 || c >= 0x00ff_fffe)
            || codes.iter().enumerate().any(|(i, c)| codes[..i].contains(c)) {
            return None;
        }
        Some(Self(codes))
    }
    pub fn operation(self, code: u32) -> Option<Operation> {
        self.0.iter().position(|&c| c == code).map(|i| [
            Operation::Request, Operation::Abandon, Operation::Unregister, Operation::Current,
        ][i])
    }
}

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
    fn remapped_transactions_do_not_dispatch_legacy_numbers() {
        let table = Transactions::new([101, 203, 307, 409]).unwrap();
        for (code, op) in [(101, Operation::Request), (203, Operation::Abandon),
            (307, Operation::Unregister), (409, Operation::Current)] {
            assert_eq!(table.operation(code), Some(op));
        }
        for code in [0, 66, 67, 68, 69] {
            assert_eq!(table.operation(code), None);
        }
        assert!(Transactions::new([1, 1, 2, 3]).is_none());
        assert!(Transactions::new([0, 1, 2, 3]).is_none());
        assert!(Transactions::new([0x00ff_fffe, 1, 2, 3]).is_none());
        assert!(Transactions::new([0x00ff_ffff, 1, 2, 3]).is_none());
        assert!(Transactions::new([0x0100_0000, 1, 2, 3]).is_none());
    }

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
