//! Minimal, dependency-free logging for the daemon.
//!
//! The daemon is a long-lived background process, so every line is prefixed
//! with a stable tag and a timestamp-free level marker. We avoid `log`/`env_logger`
//! to keep the release binary small (the ~20MB target includes no logging stack).

use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Error => "E",
            Level::Warn => "W",
            Level::Info => "I",
            Level::Debug => "D",
        }
    }
}

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Emit a formatted log line. Called by the [`info!`](crate::info) family.
pub fn emit(level: Level, args: std::fmt::Arguments<'_>) {
    if level == Level::Debug && !verbose() {
        return;
    }
    eprintln!("[pengudroid-daemon {}] {}", level.tag(), args);
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Error, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Warn, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Info, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Debug, format_args!($($arg)*)) };
}
