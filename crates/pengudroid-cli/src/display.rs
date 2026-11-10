//! The guest display's geometry: one decision, made before the app starts.
//!
//! The display used to be a fixed 720x1280 portrait panel. That is right for a
//! phone app and wrong for a game: a title whose manifest says
//! `android:screenOrientation="sensorLandscape"` (most engines, Genshin Impact
//! included) was laid out, rendered and presented in a portrait window. Android
//! itself answers this by rotating the display for the activity; there is no
//! rotation here, so the display is simply *created* in the orientation the
//! launcher activity asks for.
//!
//! The launcher decides and exports `PENGUDROID_SURFACE_W`/`_H`. Everything
//! that has to agree about the size already reads that pair — the shim's
//! `ANativeWindow`, the gleshim's surface, the app host's `DisplayInfo` — so
//! there is still exactly one place the numbers are chosen.

/// Environment override: `WIDTHxHEIGHT`, e.g. `1920x1080`.
pub const DISPLAY_ENV: &str = "PENGUDROID_DISPLAY";
/// The guest's surface size, read by the shim, the gleshim and the app host.
pub const SURFACE_W_ENV: &str = "PENGUDROID_SURFACE_W";
pub const SURFACE_H_ENV: &str = "PENGUDROID_SURFACE_H";

/// The long and short side of the default display, in pixels.
const LONG_SIDE: i32 = 1280;
const SHORT_SIDE: i32 = 720;

/// Limits for a requested size. The lower bound keeps a Material dialog (280 dp
/// at the fixed 320 dpi) on screen; the upper bound is 8K.
const MIN_SIDE: i32 = 560;
const MAX_SIDE: i32 = 7680;

/// A display size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub width: i32,
    pub height: i32,
}

impl Geometry {
    pub const PORTRAIT: Geometry = Geometry { width: SHORT_SIDE, height: LONG_SIDE };
    pub const LANDSCAPE: Geometry = Geometry { width: LONG_SIDE, height: SHORT_SIDE };

    pub fn is_landscape(self) -> bool {
        self.width > self.height
    }
}

/// Whether an `android:screenOrientation` value asks for a landscape display.
///
/// The values are `ActivityInfo.SCREEN_ORIENTATION_*`. Only the ones that
/// *require* landscape count; `unspecified`, `sensor`, `user` and friends leave
/// the choice to the device, and this device is a portrait phone by default.
pub fn wants_landscape(screen_orientation: Option<i32>) -> bool {
    const LANDSCAPE: i32 = 0;
    const SENSOR_LANDSCAPE: i32 = 6;
    const REVERSE_LANDSCAPE: i32 = 8;
    const USER_LANDSCAPE: i32 = 11;
    matches!(
        screen_orientation,
        Some(LANDSCAPE | SENSOR_LANDSCAPE | REVERSE_LANDSCAPE | USER_LANDSCAPE)
    )
}

/// Parse `WIDTHxHEIGHT`.
pub fn parse_size(text: &str) -> Result<Geometry, String> {
    let (w, h) = text
        .trim()
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("`{text}` is not WIDTHxHEIGHT"))?;
    let side = |s: &str| -> Result<i32, String> {
        let value = s
            .trim()
            .parse::<i32>()
            .map_err(|_| format!("`{s}` is not a whole number of pixels"))?;
        if !(MIN_SIDE..=MAX_SIDE).contains(&value) {
            return Err(format!("{value} is outside {MIN_SIDE}..={MAX_SIDE} pixels"));
        }
        Ok(value)
    };
    Ok(Geometry { width: side(w)?, height: side(h)? })
}

/// Choose the display for an activity.
///
/// An explicit size wins; it is the user's statement about their window. A
/// malformed one is an error rather than a silent fallback, because the result
/// would be a game at the wrong size with nothing saying why.
pub fn choose(requested: Option<&str>, screen_orientation: Option<i32>) -> Result<Geometry, String> {
    if let Some(text) = requested.filter(|t| !t.trim().is_empty()) {
        return parse_size(text).map_err(|e| format!("{DISPLAY_ENV}: {e}"));
    }
    Ok(if wants_landscape(screen_orientation) {
        Geometry::LANDSCAPE
    } else {
        Geometry::PORTRAIT
    })
}

/// One side of the surface from the environment, or `default` when it is unset
/// or not a positive number. This is the same rule the shim applies.
pub fn surface_side(var: &str, default: i32) -> i32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landscape_only_for_orientations_that_require_it() {
        for value in [0, 6, 8, 11] {
            assert!(wants_landscape(Some(value)), "{value}");
        }
        // unspecified, portrait, user, behind, sensor, nosensor, sensorPortrait,
        // reversePortrait, fullSensor, userPortrait, fullUser, locked.
        for value in [-1, 1, 2, 3, 4, 5, 7, 9, 10, 12, 13, 14] {
            assert!(!wants_landscape(Some(value)), "{value}");
        }
        assert!(!wants_landscape(None));
    }

    #[test]
    fn manifest_orientation_picks_the_display() {
        assert_eq!(choose(None, Some(6)), Ok(Geometry::LANDSCAPE));
        assert_eq!(choose(None, Some(1)), Ok(Geometry::PORTRAIT));
        assert_eq!(choose(None, None), Ok(Geometry::PORTRAIT));
        assert_eq!(choose(Some("  "), Some(6)), Ok(Geometry::LANDSCAPE));
        assert!(Geometry::LANDSCAPE.is_landscape());
        assert!(!Geometry::PORTRAIT.is_landscape());
    }

    #[test]
    fn an_explicit_size_wins_and_is_validated() {
        assert_eq!(
            choose(Some("1920x1080"), Some(1)),
            Ok(Geometry { width: 1920, height: 1080 })
        );
        assert_eq!(parse_size(" 720X1280 "), Ok(Geometry::PORTRAIT));
        for bad in ["1920", "1920x", "x1080", "axb", "100x100", "99999x1080", "-1280x720"] {
            assert!(choose(Some(bad), None).is_err(), "{bad}");
        }
    }
}
