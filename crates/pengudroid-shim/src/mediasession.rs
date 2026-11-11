//! Hosted `ISession` setters. The per-process media session has no
//! `system_server` peer, so the framework's setters update local state here
//! instead of being forwarded. Anything that would report real system state —
//! controller playback state, queue transfer — stays unimplemented.
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// Semantic `ISession` operations, independent of generated AIDL codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Connect,
    Disconnect,
    SetFlags,
    SetMetadata,
    SetPlaybackState,
    SetSessionActivity,
    SetSessionExtras,
    SetPlaybackToLocal,
    SetPlaybackToRemote,
    SetCurrentVolume,
    /// The only getter answered from local state.
    IsPlaybackActive,
}

/// Order of the codes passed to [`Transactions::new`], and of the
/// `TRANSACTION_*` fields the host reads from `android.media.session.ISession$Stub`.
pub const OPERATIONS: [Operation; 11] = [
    Operation::Connect,
    Operation::Disconnect,
    Operation::SetFlags,
    Operation::SetMetadata,
    Operation::SetPlaybackState,
    Operation::SetSessionActivity,
    Operation::SetSessionExtras,
    Operation::SetPlaybackToLocal,
    Operation::SetPlaybackToRemote,
    Operation::SetCurrentVolume,
    Operation::IsPlaybackActive,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transactions([u32; OPERATIONS.len()]);

impl Transactions {
    /// Validate codes read from the loaded API-37 `ISession$Stub`, in
    /// [`OPERATIONS`] order. Zero marks a method the loaded framework does not
    /// declare (no dispatch for it); every other code must be distinct and in
    /// `1..0x00ff_fffe`.
    pub fn new(codes: [u32; OPERATIONS.len()]) -> Option<Self> {
        if codes.iter().any(|&c| c >= 0x00ff_fffe) {
            return None;
        }
        for (i, &code) in codes.iter().enumerate() {
            if code != 0 && codes[..i].contains(&code) {
                return None;
            }
        }
        Some(Self(codes))
    }

    pub fn operation(self, code: u32) -> Option<Operation> {
        self.0
            .iter()
            .position(|&c| c == code && c != 0)
            .map(|i| OPERATIONS[i])
    }
}

/// State the setters record. The hosted session is not arbitrated by
/// `system_server`, so this is what the app told us, not what the device decided.
#[derive(Debug)]
pub struct Session {
    active: AtomicBool,
    flags: AtomicI32,
    volume: AtomicI32,
}

impl Session {
    pub const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            flags: AtomicI32::new(0),
            volume: AtomicI32::new(0),
        }
    }

    pub fn apply(&self, op: Operation, volume: i32, flags: i32) {
        match op {
            Operation::Connect => self.active.store(true, Ordering::Relaxed),
            Operation::Disconnect => self.active.store(false, Ordering::Relaxed),
            Operation::SetFlags => self.flags.store(flags, Ordering::Relaxed),
            Operation::SetPlaybackState => self.active.store(true, Ordering::Relaxed),
            Operation::SetCurrentVolume => self.volume.store(volume, Ordering::Relaxed),
            Operation::SetMetadata
            | Operation::SetSessionActivity
            | Operation::SetSessionExtras
            | Operation::SetPlaybackToLocal
            | Operation::SetPlaybackToRemote
            | Operation::IsPlaybackActive => {}
        }
    }

    pub fn playback_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn flags(&self) -> i32 {
        self.flags.load(Ordering::Relaxed)
    }

    pub fn volume(&self) -> i32 {
        self.volume.load(Ordering::Relaxed)
    }

    /// Forget everything a disconnect ends, so a reused handle does not report
    /// the previous app's state.
    pub fn reset(&self) {
        self.active.store(false, Ordering::Relaxed);
        self.flags.store(0, Ordering::Relaxed);
        self.volume.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_select_operations_and_absent_methods_never_dispatch() {
        let codes = [11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21];
        let table = Transactions::new(codes).unwrap();
        for (code, op) in codes.iter().zip(OPERATIONS) {
            assert_eq!(table.operation(*code), Some(op));
        }
        assert_eq!(table.operation(0), None);
        assert_eq!(table.operation(22), None);

        // A framework revision without a method passes 0 for that slot.
        let mut sparse = codes;
        sparse[4] = 0;
        let table = Transactions::new(sparse).unwrap();
        assert_eq!(table.operation(0), None);
        assert_eq!(table.operation(15), None);
        assert_eq!(table.operation(16), Some(Operation::SetSessionActivity));

        assert!(Transactions::new([1; 11]).is_none());
        assert!(Transactions::new([0; 11]).is_some());
        assert!(Transactions::new([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0x00ff_fffe]).is_none());
        let mut repeated_zero = [0; 11];
        repeated_zero[3] = 1;
        repeated_zero[5] = 1;
        assert!(Transactions::new(repeated_zero).is_none());
    }

    #[test]
    fn setters_record_only_what_the_app_reported() {
        let session = Session::new();
        assert!(!session.playback_active());
        session.apply(Operation::Connect, 0, 0);
        assert!(session.playback_active());
        session.apply(Operation::SetFlags, 0, 7);
        assert_eq!(session.flags(), 7);
        session.apply(Operation::SetCurrentVolume, 3, 0);
        assert_eq!(session.volume(), 3);
        // Getters that would need a real session change nothing.
        session.apply(Operation::SetMetadata, 9, 9);
        assert_eq!(session.volume(), 3);
        assert_eq!(session.flags(), 7);
        session.apply(Operation::Disconnect, 0, 0);
        assert!(!session.playback_active());
        session.apply(Operation::SetPlaybackState, 0, 0);
        assert!(session.playback_active());
        session.reset();
        assert!(!session.playback_active());
        assert_eq!((session.flags(), session.volume()), (0, 0));
    }
}
