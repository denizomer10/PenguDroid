//! Stub implementations of the Android platform services.
//!
//! These exist so that guests which probe for `phone`, `media.camera`, or the
//! activity manager get a well-formed, honest answer instead of a hang.
//!
//! **Telephony is the exception.** `ITelephony` uses its real AIDL transaction
//! codes and marshals replies in the real `Parcel` format (see
//! [`crate::parcel`]), modelling a no-SIM, radio-off device. The remaining
//! services are still stubs: their transaction codes are illustrative and the
//! replies are minimal. Real implementations would marshal `Parcel` bytes to
//! match each AIDL method exactly.
//!
//! Marked clearly so nobody mistakes these for working camera/AMS/PMS.

use crate::parcel::ParcelWriter;
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
// Telephony
// ---------------------------------------------------------------------------
//
// `ITelephony` is a real AIDL interface, so its transaction codes are not free
// choices: the AIDL compiler numbers methods in declaration order starting at
// `FIRST_CALL_TRANSACTION` (1). The constants below are that numbering for the
// subset of methods a normal app reaches through `TelephonyManager`, taken from
// `frameworks/base/telephony/java/com/android/internal/telephony/ITelephony.aidl`.
//
// The numbering is stable for a given AOSP release but can shift when methods
// are inserted, so each constant names its method and the replies are marshalled
// with the real `Parcel` format (see [`crate::parcel`]).
//
// The device modelled is a **no-SIM, radio-off** phone: every query answers the
// inert value a device without a SIM reports, which is what lets apps that probe
// telephony degrade gracefully instead of throwing.

/// `isRadioOn(String callingPackage)`.
pub const TELEPHONY_IS_RADIO_ON: u32 = 3;
/// `getCallState()`.
pub const TELEPHONY_GET_CALL_STATE: u32 = 34;
/// `getDataActivity()`.
pub const TELEPHONY_GET_DATA_ACTIVITY: u32 = 36;
/// `getDataState()`.
pub const TELEPHONY_GET_DATA_STATE: u32 = 38;
/// `getActivePhoneType()`.
pub const TELEPHONY_GET_ACTIVE_PHONE_TYPE: u32 = 40;
/// `getNetworkTypeForSubscriber(int, String, String)`.
pub const TELEPHONY_GET_NETWORK_TYPE: u32 = 64;
/// `getDataNetworkType(String, String)`.
pub const TELEPHONY_GET_DATA_NETWORK_TYPE: u32 = 65;
/// `getVoiceNetworkTypeForSubscriber(int, String, String)`.
pub const TELEPHONY_GET_VOICE_NETWORK_TYPE: u32 = 67;
/// `hasIccCard()`.
pub const TELEPHONY_HAS_ICC_CARD: u32 = 68;
/// `hasIccCardUsingSlotIndex(int)`.
pub const TELEPHONY_HAS_ICC_CARD_SLOT: u32 = 69;
/// `getAllCellInfo(String, String)`.
pub const TELEPHONY_GET_ALL_CELL_INFO: u32 = 72;
/// `getDeviceId(String)`.
pub const TELEPHONY_GET_DEVICE_ID: u32 = 146;
/// `getDeviceIdWithFeature(String, String)`.
pub const TELEPHONY_GET_DEVICE_ID_WITH_FEATURE: u32 = 147;
/// `getImeiForSlot(int, String, String)`.
pub const TELEPHONY_GET_IMEI_FOR_SLOT: u32 = 148;
/// `getPrimaryImei(String, String)`.
pub const TELEPHONY_GET_PRIMARY_IMEI: u32 = 149;
/// `getMeidForSlot(int, String, String)`.
pub const TELEPHONY_GET_MEID_FOR_SLOT: u32 = 151;
/// `getServiceStateForSlot(int, boolean, boolean, String, String)`.
pub const TELEPHONY_GET_SERVICE_STATE: u32 = 159;
/// `getSignalStrength(int)`.
pub const TELEPHONY_GET_SIGNAL_STRENGTH: u32 = 190;

/// `TelephonyManager.CALL_STATE_IDLE`.
const CALL_STATE_IDLE: i32 = 0;
/// `TelephonyManager.DATA_ACTIVITY_NONE`.
const DATA_ACTIVITY_NONE: i32 = 0;
/// `TelephonyManager.DATA_DISCONNECTED`.
const DATA_DISCONNECTED: i32 = 0;
/// `TelephonyManager.PHONE_TYPE_GSM`.
const PHONE_TYPE_GSM: i32 = 1;
/// `TelephonyManager.NETWORK_TYPE_UNKNOWN`.
const NETWORK_TYPE_UNKNOWN: i32 = 0;

fn bool_reply(v: bool) -> Reply {
    let mut w = ParcelWriter::new();
    w.write_bool(v);
    Reply::ok(w.into_bytes())
}

fn null_string_reply() -> Reply {
    let mut w = ParcelWriter::new();
    w.write_string16(None);
    Reply::ok(w.into_bytes())
}

fn null_parcelable_reply() -> Reply {
    let mut w = ParcelWriter::new();
    w.write_null_parcelable();
    Reply::ok(w.into_bytes())
}

fn empty_list_reply() -> Reply {
    let mut w = ParcelWriter::new();
    w.write_empty_list();
    Reply::ok(w.into_bytes())
}

/// Answer an `ITelephony` transaction for a no-SIM, radio-off device.
///
/// The request `data` is not inspected: none of the implemented methods changes
/// its answer based on its arguments, and the reply is a well-formed `Parcel`
/// regardless. Methods outside the implemented subset return `-ENOSYS` so the
/// caller sees an explicit "unsupported" rather than a malformed reply.
pub fn telephony_transact(code: u32, _data: &[u8]) -> Reply {
    match code {
        TELEPHONY_IS_RADIO_ON => bool_reply(false),
        TELEPHONY_GET_CALL_STATE => i32_reply(CALL_STATE_IDLE),
        TELEPHONY_GET_DATA_ACTIVITY => i32_reply(DATA_ACTIVITY_NONE),
        TELEPHONY_GET_DATA_STATE => i32_reply(DATA_DISCONNECTED),
        TELEPHONY_GET_ACTIVE_PHONE_TYPE => i32_reply(PHONE_TYPE_GSM),
        TELEPHONY_GET_NETWORK_TYPE
        | TELEPHONY_GET_DATA_NETWORK_TYPE
        | TELEPHONY_GET_VOICE_NETWORK_TYPE => i32_reply(NETWORK_TYPE_UNKNOWN),
        TELEPHONY_HAS_ICC_CARD | TELEPHONY_HAS_ICC_CARD_SLOT => bool_reply(false),
        TELEPHONY_GET_ALL_CELL_INFO => empty_list_reply(),
        TELEPHONY_GET_DEVICE_ID
        | TELEPHONY_GET_DEVICE_ID_WITH_FEATURE
        | TELEPHONY_GET_IMEI_FOR_SLOT
        | TELEPHONY_GET_PRIMARY_IMEI
        | TELEPHONY_GET_MEID_FOR_SLOT => null_string_reply(),
        TELEPHONY_GET_SERVICE_STATE | TELEPHONY_GET_SIGNAL_STRENGTH => null_parcelable_reply(),
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
        // `isRadioOn` returns a marshalled `boolean` (a 32-bit 0).
        assert_eq!(telephony_transact(TELEPHONY_IS_RADIO_ON, &[]), bool_reply(false));
    }

    #[test]
    fn telephony_reports_a_no_sim_device() {
        assert_eq!(telephony_transact(TELEPHONY_GET_CALL_STATE, &[]), i32_reply(0));
        assert_eq!(telephony_transact(TELEPHONY_HAS_ICC_CARD, &[]), bool_reply(false));
        assert_eq!(telephony_transact(TELEPHONY_GET_ACTIVE_PHONE_TYPE, &[]), i32_reply(1));
        // A device id is a `null` string (length -1).
        assert_eq!(
            telephony_transact(TELEPHONY_GET_DEVICE_ID, &[]).data,
            (-1i32).to_le_bytes().to_vec()
        );
        // Cell info is an empty list.
        assert_eq!(telephony_transact(TELEPHONY_GET_ALL_CELL_INFO, &[]).data, 0i32.to_le_bytes().to_vec());
    }

    #[test]
    fn telephony_unknown_code_is_unsupported() {
        assert_eq!(telephony_transact(9999, &[]), Reply::unsupported());
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
