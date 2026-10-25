//! PenguDroid daemon library.
//!
//! The daemon is split into a library plus a thin binary so its components can
//! be unit- and integration-tested directly, and so tooling (the CLI, future
//! probes) can link the broker without shelling out to the binary.
//!
//! Module map:
//! * [`server`]   — shared state, accept loop, message dispatch
//! * [`graphics`] — Wayland translation of the Android surface model
//! * [`binder`]   — kernel binder probe + userspace broker wiring
//! * [`services`] — the service registry and transaction routing
//! * [`ashmem`]   — `memfd`-backed ashmem regions
//! * [`audio`]    — PipeWire ring buffer
//! * [`props`]    — the Android property store
//! * [`parcel`]   — the Android `Parcel` codec used by the stub services
//! * [`stubs`]    — telephony/camera/AMS/PMS/SF placeholders
//! * [`log`]      — tiny leveled logger

pub mod ashmem;
pub mod audio;
pub mod binder;
pub mod graphics;
pub mod log;
pub mod parcel;
pub mod props;
pub mod server;
pub mod services;
pub mod stubs;
mod software;

pub use server::Daemon;
