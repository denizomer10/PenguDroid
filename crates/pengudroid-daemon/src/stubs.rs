//! Stub implementations of the Android platform services.
//!
//! These exist so that guests which probe for `phone`, `media.camera`, or the
//! activity manager get a well-formed, honest answer instead of a hang. Every
//! function here is a **stub**: the transaction codes are illustrative of the
//! real AIDL interfaces but the replies are minimal (typically "unsupported"
//! or an empty result). Real implementations would marshal `Parcel` bytes to
//! match each AIDL method exactly.
//!
//! Marked clearly so nobody mistakes these for working telephony/camera.

use crate::services::{Reply, ServiceKind};

/// The set of platform services PenguDroid advertises, as
/// `(name, interface, kind)`.
pub fn system_services() -> Vec<(&'static str, &'static str, ServiceKind)> {
    vec![
        ("activity", "android.app.IActivityManager", ServiceKind::ActivityManager),
        ("package", "android.content.pm.IPackageManager", ServiceKind::PackageManager),
        ("window", "android.view.IWindowManager", ServiceKind::WindowManager),
        ("SurfaceFlinger", "android.gui.ISurfaceComposer", ServiceKind::SurfaceComposer),
        ("media.camera", "android.hardware.ICameraService", ServiceKind::Camera),
        ("media.audio_flinger", "android.media.IAudioFlinger", ServiceKind::AudioFlinger),
        ("phone", "com.android.internal.telephony.ITelephony", ServiceKind::Telephony),
        ("input", "android.hardware.input.IInputFlinger", ServiceKind::InputFlinger),
    ]
}

/// Largest parcel we will echo back, to bound a hostile/buggy request loop.
const MAX_REPLY: usize = 4096;

/// Helper: an empty Parcel reply (the common "nothing to return" case).
fn empty_ok() -> Reply {
    Reply::ok(Vec::new())
}

fn i32_reply(v: i32) -> Reply {
    Reply::ok(v.to_le_bytes().to_vec())
}

// ---------------------------------------------------------------------------
// Telephony (STUB)
// ---------------------------------------------------------------------------

/// Illustrative `ITelephony` transaction codes.
pub const TELEPHONY_GET_CALL_STATE: u32 = 1;
pub const TELEPHONY_END_CALL: u32 = 2;
pub const TELEPHONY_IS_RADIO_ON: u32 = 3;
pub const TELEPHONY_GET_DEVICE_ID: u32 = 4;

/// A no-SIM device: radio off, no calls. This is deliberately inert so that
/// apps relying on telephony degrade gracefully (many check `isRadioOn` first).
pub fn telephony_transact(code: u32, _data: &[u8]) -> Reply {
    match code {
        TELEPHONY_IS_RADIO_ON | TELEPHONY_GET_CALL_STATE => i32_reply(0),
        TELEPHONY_END_CALL => empty_ok(),
        TELEPHONY_GET_DEVICE_ID => Reply::unsupported(),
        _ => Reply::unsupported(),
    }
}

// ---------------------------------------------------------------------------
// Camera (STUB)
// ---------------------------------------------------------------------------

pub const CAMERA_GET_NUMBER_OF_CAMERAS: u32 = 1;
pub const CAMERA_GET_CAMERA_INFO: u32 = 2;
pub const CAMERA_CONNECT: u32 = 3;

/// Zero cameras. `CameraManager.getCameraIdList()` will therefore return an
/// empty array rather than throwing, which is exactly what a device without a
/// camera does.
pub fn camera_transact(code: u32, _data: &[u8]) -> Reply {
    match code {
        CAMERA_GET_NUMBER_OF_CAMERAS => i32_reply(0),
        CAMERA_GET_CAMERA_INFO | CAMERA_CONNECT => Reply::unsupported(),
        _ => Reply::unsupported(),
    }
}

// ---------------------------------------------------------------------------
// ActivityManager (STUB)
// ---------------------------------------------------------------------------

pub const AM_START_ACTIVITY: u32 = 1;
pub const AM_GET_TASKS: u32 = 2;
pub const AM_FORCE_STOP_PACKAGE: u32 = 3;

pub fn activity_manager_transact(code: u32, _data: &[u8]) -> Reply {
    match code {
        AM_START_ACTIVITY => {
            // Report "started" (status 0) but do not actually launch anything
            // yet: the MVP `pengudroid run` path drives that from the CLI.
            empty_ok()
        }
        AM_FORCE_STOP_PACKAGE => empty_ok(),
        AM_GET_TASKS => Reply::ok(0i32.to_le_bytes().to_vec()), // 0 tasks
        _ => Reply::unsupported(),
    }
}

// ---------------------------------------------------------------------------
// PackageManager (STUB)
// ---------------------------------------------------------------------------

pub const PM_GET_PACKAGE_INFO: u32 = 1;
pub const PM_RESOLVE_INTENT: u32 = 2;
pub const PM_GET_INSTALLED_PACKAGES: u32 = 3;

pub fn package_manager_transact(code: u32, _data: &[u8]) -> Reply {
    match code {
        PM_GET_INSTALLED_PACKAGES => Reply::ok(0i32.to_le_bytes().to_vec()),
        PM_GET_PACKAGE_INFO | PM_RESOLVE_INTENT => Reply::unsupported(),
        _ => Reply::unsupported(),
    }
}

// ---------------------------------------------------------------------------
// SurfaceFlinger (STUB)
// ---------------------------------------------------------------------------

/// In the real system SurfaceFlinger composites layers. PenguDroid does **not**
/// route apps through SurfaceFlinger at all: the graphics broker
/// (see [`crate::graphics`]) translates `BufferQueue`/EGL usage straight to
/// Wayland. These handlers only exist to answer introspection calls.
pub const SF_GET_DISPLAY_INFO: u32 = 1;
pub const SF_CREATE_SURFACE: u32 = 2;

pub fn surface_composer_transact(code: u32, data: &[u8]) -> Reply {
    match code {
        SF_GET_DISPLAY_INFO => {
            // width:u32, height:u32, refresh_mhz:u32 — a plausible 1920×1080@60.
            let mut out = Vec::new();
            out.extend_from_slice(&1920u32.to_le_bytes());
            out.extend_from_slice(&1080u32.to_le_bytes());
            out.extend_from_slice(&60000u32.to_le_bytes());
            Reply::ok(out)
        }
        SF_CREATE_SURFACE => {
            // Echo back a surface token derived from the request payload.
            let token = data.iter().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(*b as u32));
            Reply::ok(token.to_le_bytes().to_vec())
        }
        _ => Reply::unsupported(),
    }
}

/// Clamp a reply payload so a peer cannot force unbounded memory use.
pub fn clamp(reply: Reply) -> Reply {
    if reply.data.len() > MAX_REPLY {
        Reply { status: reply.status, data: reply.data[..MAX_REPLY].to_vec() }
    } else {
        reply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telephony_radio_is_off() {
        assert_eq!(telephony_transact(TELEPHONY_IS_RADIO_ON, &[]), i32_reply(0));
    }

    #[test]
    fn surface_composer_returns_display() {
        let r = surface_composer_transact(SF_GET_DISPLAY_INFO, &[]);
        assert_eq!(r.data.len(), 12);
        assert_eq!(&r.data[0..4], &1920u32.to_le_bytes());
    }

    #[test]
    fn clamp_truncates() {
        let big = Reply::ok(vec![0u8; MAX_REPLY + 100]);
        assert_eq!(clamp(big).data.len(), MAX_REPLY);
    }
}
