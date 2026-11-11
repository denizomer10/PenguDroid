//! The display geometry the guest is told it has.
//!
//! One policy, two consumers: the guest launcher sizes the Wayland window and
//! exports `PENGUDROID_SURFACE_W`/`_H`/`_DENSITY_DPI`, and `apphost` reads those
//! back when it builds `DisplayInfo`, `Configuration` and the root window frame.
//! Keeping them in one place is what makes `--width`/`--height` mean the same
//! thing to the compositor and to the framework.
//!
//! Two things are chosen here rather than hardcoded at the call sites:
//!
//! * **Density.** `dp = px * 160 / dpi`, so the density decides how much UI fits
//!   in a given window. A window at 160 dpi reports twice the dp of the same
//!   pixels at 320 dpi.
//! * **Tablet mode.** The framework switches to tablet layouts when
//!   `smallestScreenWidthDp >= 600`. Android decides that from the *window*, so a
//!   large window must be paired with a density low enough to cross 600 dp —
//!   otherwise a maximized window would still lay out like a phone.

/// The surface size used when the caller does not ask for one.
pub const DEFAULT_WIDTH: u32 = 720;
pub const DEFAULT_HEIGHT: u32 = 1280;

/// Density of the default phone surface (360x640 dp).
pub const PHONE_DENSITY: i32 = 320;

/// `smallestScreenWidthDp` at which the framework picks tablet layouts.
pub const TABLET_MIN_DP: i32 = 600;

/// A Material `AlertDialog` is 280 dp wide; anything narrower is clipped.
pub const MIN_USABLE_DP: i32 = 280;

/// Densities considered when nothing was requested, from densest to sparsest.
const DENSITY_LADDER: [i32; 3] = [320, 240, 160];

/// Smallest and largest accepted density, in dpi.
const MIN_DENSITY: i32 = 120;
const MAX_DENSITY: i32 = 640;

/// Smallest and largest accepted surface size, in pixels.
const MIN_SIDE: u32 = 320;
const MAX_SIDE: u32 = 8192;

/// A resolved surface geometry: pixel size plus density.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: i32,
    pub height: i32,
    pub density_dpi: i32,
}

impl Geometry {
    /// The default: a conventional small phone, 360x640 dp at 320 dpi.
    pub fn phone() -> Self {
        Self {
            width: DEFAULT_WIDTH as i32,
            height: DEFAULT_HEIGHT as i32,
            density_dpi: PHONE_DENSITY,
        }
    }

    /// Resolve a requested window size into a layout geometry.
    ///
    /// `density` is honoured when given. Otherwise the density is picked so the
    /// window reports a sensible dp size: large/landscape windows become
    /// tablets (>= [`TABLET_MIN_DP`] dp on the short side), and narrow windows
    /// drop density until the dp width clears [`MIN_USABLE_DP`].
    pub fn for_window(width: u32, height: u32, density: Option<i32>) -> Self {
        let width = width.clamp(MIN_SIDE, MAX_SIDE);
        let height = height.clamp(MIN_SIDE, MAX_SIDE);
        let density_dpi = match density {
            Some(dpi) => dpi.clamp(MIN_DENSITY, MAX_DENSITY),
            None => auto_density(width, height),
        };
        Self {
            width: width as i32,
            height: height as i32,
            density_dpi,
        }
    }

    /// Recompute this geometry for a new window size after a host resize.
    ///
    /// The density is *kept*: re-deriving it mid-session would rescale the whole
    /// UI (a 720x1280 window at 320 dpi is 360x640 dp; the same window at 160 dpi
    /// would be 720x1280 dp, i.e. every touch target halves) under the user's
    /// finger. Keeping it also matches what the compositor does to the frame:
    /// the guest keeps rendering at its launch-time pixel size and the
    /// destination rectangle is what grows or shrinks.
    ///
    /// Only the dp size, and therefore [`Geometry::is_tablet`], follows the
    /// window. Whether a resized window becomes tablet-class is a function of
    /// the window's pixel size *and* the density chosen at launch, which is the
    /// same rule Android applies (`smallestScreenWidthDp` comes from the
    /// configuration, the density does not change with the window).
    pub fn for_resize(&self, width: u32, height: u32) -> Self {
        Self::for_window(width, height, Some(self.density_dpi))
    }

    /// The smallest window side, in pixels, that this density reports as
    /// tablet-sized.
    ///
    /// A window below this can only be tablet-class at a sparser density, and a
    /// sparser density is a launch-time choice. `None` means the density is so
    /// dense that no accepted window size reaches [`TABLET_MIN_DP`].
    pub fn tablet_side_px(&self) -> Option<u32> {
        let side = (TABLET_MIN_DP as i64 * self.density_dpi as i64 / 160) as u32;
        (side <= MAX_SIDE).then_some(side)
    }

    /// Read the geometry the launcher exported.
    ///
    /// Falls back to [`Geometry::phone`] when the size is not in the
    /// environment, which is what the standalone `art_test` path sees.
    pub fn from_env() -> Self {
        let width = env_int("PENGUDROID_SURFACE_W");
        let height = env_int("PENGUDROID_SURFACE_H");
        let density = env_int("PENGUDROID_DENSITY_DPI");
        match (width, height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Self::for_window(w as u32, h as u32, density),
            _ => {
                let mut g = Self::phone();
                if let Some(dpi) = density {
                    g.density_dpi = dpi.clamp(MIN_DENSITY, MAX_DENSITY);
                }
                g
            }
        }
    }

    /// Write this geometry to the environment for the guest and the shim.
    pub fn export(&self) -> [(&'static str, String); 3] {
        [
            ("PENGUDROID_SURFACE_W", self.width.to_string()),
            ("PENGUDROID_SURFACE_H", self.height.to_string()),
            ("PENGUDROID_DENSITY_DPI", self.density_dpi.to_string()),
        ]
    }

    /// Width in dp, the unit `Configuration.screenWidthDp` uses.
    pub fn width_dp(&self) -> i32 {
        dp(self.width, self.density_dpi)
    }

    /// Height in dp.
    pub fn height_dp(&self) -> i32 {
        dp(self.height, self.density_dpi)
    }

    /// `Configuration.smallestScreenWidthDp`.
    pub fn smallest_width_dp(&self) -> i32 {
        self.width_dp().min(self.height_dp())
    }

    /// Whether the framework should use tablet layouts for this geometry.
    pub fn is_tablet(&self) -> bool {
        self.smallest_width_dp() >= TABLET_MIN_DP
    }

    /// `DisplayMetrics.density`, the dp-to-px scale factor.
    pub fn density_scale(&self) -> f32 {
        self.density_dpi as f32 / 160.0
    }
}

impl Default for Geometry {
    fn default() -> Self {
        Self::phone()
    }
}

/// `pengudroid geometry [--width W] [--height H] [--density DPI] [--resize WxH]`
///
/// Prints the resolved geometry, and with `--resize` the classification the same
/// session would report after the window is resized to that size. This is the
/// only way to see the resize policy without a running compositor: it answers
/// "will this window be tablet-class, and at what density".
pub fn run(args: &[String]) -> i32 {
    let mut width = env_int("PENGUDROID_SURFACE_W").map(|v| v as u32);
    let mut height = env_int("PENGUDROID_SURFACE_H").map(|v| v as u32);
    let mut density = env_int("PENGUDROID_DENSITY_DPI");
    let mut resize: Option<(u32, u32)> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let value = args.get(i + 1);
        match arg {
            "--width" => width = value.and_then(|v| v.parse().ok()),
            "--height" => height = value.and_then(|v| v.parse().ok()),
            "--density" => density = value.and_then(|v| v.parse().ok()),
            "--resize" => {
                let spec = value.map(String::as_str).unwrap_or_default();
                resize = spec.split_once(['x', 'X']).and_then(|(w, h)| {
                    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
                });
                if resize.is_none() {
                    eprintln!("pengudroid geometry: --resize wants WxH, got {spec:?}");
                    return 2;
                }
            }
            "-h" | "--help" => {
                println!(
                    "usage: pengudroid geometry [--width W] [--height H] [--density DPI]\n\
                     \x20                                [--resize WxH]"
                );
                return 0;
            }
            other => {
                eprintln!("pengudroid geometry: unknown argument {other:?}");
                return 2;
            }
        }
        i += 2;
    }

    let geometry = match (width, height) {
        (Some(w), Some(h)) => Geometry::for_window(w, h, density),
        _ => Geometry::from_env(),
    };
    println!(
        "{}x{} px at {} dpi -> {}x{} dp, smallestScreenWidthDp {}, {}",
        geometry.width,
        geometry.height,
        geometry.density_dpi,
        geometry.width_dp(),
        geometry.height_dp(),
        geometry.smallest_width_dp(),
        if geometry.is_tablet() { "tablet" } else { "phone" }
    );
    if let Some(side) = geometry.tablet_side_px() {
        println!("tablet layouts start at a {side} px window side at this density");
    } else {
        println!("no accepted window size reaches {TABLET_MIN_DP} dp at this density");
    }
    if let Some((w, h)) = resize {
        let resized = geometry.for_resize(w, h);
        println!(
            "resized to {}x{} px -> {}x{} dp, smallestScreenWidthDp {}, {} (density kept at {} dpi)",
            resized.width,
            resized.height,
            resized.width_dp(),
            resized.height_dp(),
            resized.smallest_width_dp(),
            if resized.is_tablet() { "tablet" } else { "phone" },
            resized.density_dpi
        );
        if resized.is_tablet() != geometry.is_tablet() {
            println!(
                "  note: the guest keeps rendering at {}x{} px; the compositor scales that frame \
                 into the new window, and the app is not told about the size change",
                geometry.width, geometry.height
            );
        }
    }
    0
}

fn dp(px: i32, density_dpi: i32) -> i32 {
    px * 160 / density_dpi.max(MIN_DENSITY)
}

fn env_int(name: &str) -> Option<i32> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// Pick a density for a window nobody specified one for.
fn auto_density(width: u32, height: u32) -> i32 {
    let (w, h) = (width as i32, height as i32);
    // A tall, phone-shaped window keeps the phone density even when a sparser
    // density would push it past the tablet threshold — a 1080x1920 phone is
    // still a phone, and reporting 240 dpi there would shrink its text. Only a
    // portrait window that is already 600 dp wide at 320 dpi is tablet-class.
    let portrait_phone = h * 3 >= w * 4 && dp(w, PHONE_DENSITY) < TABLET_MIN_DP;
    if portrait_phone {
        return DENSITY_LADDER
            .iter()
            .copied()
            .find(|dpi| dp(w, *dpi) >= MIN_USABLE_DP)
            .unwrap_or(160);
    }
    // Otherwise prefer the densest setting that still reports a tablet-sized
    // screen, then the densest that keeps the window usable at all.
    DENSITY_LADDER
        .iter()
        .copied()
        .find(|dpi| dp(w, *dpi).min(dp(h, *dpi)) >= TABLET_MIN_DP)
        .or_else(|| {
            DENSITY_LADDER
                .iter()
                .copied()
                .find(|dpi| dp(w, *dpi) >= MIN_USABLE_DP)
        })
        .unwrap_or(160)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_a_usable_phone() {
        let g = Geometry::phone();
        assert_eq!((g.width, g.height, g.density_dpi), (720, 1280, 320));
        assert_eq!((g.width_dp(), g.height_dp()), (360, 640));
        assert_eq!(g.smallest_width_dp(), 360);
        assert!(!g.is_tablet());
        // Round-trips back to the pixel size.
        assert_eq!(g.width_dp() * g.density_dpi / 160, g.width);
        assert_eq!(g.height_dp() * g.density_dpi / 160, g.height);
    }

    #[test]
    fn a_portrait_phone_stays_a_phone() {
        for (w, h) in [(720, 1280), (1080, 1920), (720, 1600)] {
            let g = Geometry::for_window(w, h, None);
            assert_eq!(g.density_dpi, 320, "{w}x{h}");
            assert!(!g.is_tablet(), "{w}x{h} should not be tablet");
            assert!(g.width_dp() >= MIN_USABLE_DP, "{w}x{h} too narrow");
        }
    }

    #[test]
    fn a_large_window_becomes_a_tablet() {
        for (w, h) in [(1024, 768), (1280, 800), (2560, 1600), (1600, 2560)] {
            let g = Geometry::for_window(w, h, None);
            assert!(g.is_tablet(), "{w}x{h} -> {g:?}");
        }
        // The densest setting that still reaches tablet dp wins: 1024 px only
        // crosses 600 dp at 160 dpi, 2560 px already does at 320.
        assert_eq!(Geometry::for_window(1024, 768, None).density_dpi, 160);
        assert_eq!(Geometry::for_window(2560, 1600, None).density_dpi, 320);
        // A portrait tablet is tablet-class without lowering the density.
        assert_eq!(Geometry::for_window(1600, 2560, None).density_dpi, 320);
    }

    #[test]
    fn a_narrow_window_drops_density_instead_of_clipping_dialogs() {
        let g = Geometry::for_window(480, 800, None);
        assert!(g.width_dp() >= MIN_USABLE_DP, "{g:?}");
        assert_eq!(g.density_dpi, 240);
    }

    #[test]
    fn a_requested_density_wins_and_is_clamped() {
        assert_eq!(Geometry::for_window(1024, 768, Some(320)).density_dpi, 320);
        assert!(!Geometry::for_window(1024, 768, Some(320)).is_tablet());
        assert_eq!(Geometry::for_window(720, 1280, Some(10)).density_dpi, MIN_DENSITY);
        assert_eq!(Geometry::for_window(720, 1280, Some(4000)).density_dpi, MAX_DENSITY);
    }

    #[test]
    fn sizes_are_clamped_to_something_renderable() {
        let tiny = Geometry::for_window(0, 0, None);
        assert_eq!((tiny.width, tiny.height), (MIN_SIDE as i32, MIN_SIDE as i32));
        let huge = Geometry::for_window(u32::MAX, u32::MAX, None);
        assert_eq!((huge.width, huge.height), (MAX_SIDE as i32, MAX_SIDE as i32));
    }

    #[test]
    fn a_resize_keeps_the_density_and_the_dp_size_follows() {
        let launch = Geometry::for_window(720, 1280, None);
        assert_eq!(launch.density_dpi, PHONE_DENSITY);
        // A resized window never changes the UI scale.
        for (w, h) in [(320, 320), (1600, 1000), (2560, 1600), (8192, 8192)] {
            assert_eq!(launch.for_resize(w, h).density_dpi, launch.density_dpi, "{w}x{h}");
        }
        // Shrinking keeps the phone classification; growing past the density's
        // tablet side makes it tablet-class.
        let small = launch.for_resize(480, 800);
        assert!(!small.is_tablet());
        assert_eq!((small.width, small.height), (480, 800));
        let big = launch.for_resize(1280, 1600);
        assert!(big.is_tablet(), "{big:?}");
        // Same geometry a launch with that density would have produced.
        assert_eq!(big, Geometry::for_window(1280, 1600, Some(PHONE_DENSITY)));
    }

    #[test]
    fn a_resize_never_flips_a_phone_into_tablet_text() {
        // 600 dp needs 1200 px at 320 dpi, so a window one pixel short of that
        // stays a phone: the launch-time density is kept, not re-derived.
        let launch = Geometry::for_window(720, 1280, None);
        assert_eq!(launch.tablet_side_px(), Some(1200));
        let near = launch.for_resize(1199, 1920);
        assert_eq!((near.width_dp(), near.is_tablet()), (599, false), "{near:?}");
        let past = launch.for_resize(1200, 1920);
        assert_eq!((past.width_dp(), past.is_tablet()), (600, true), "{past:?}");
        // The tablet side tracks the density rather than being a constant.
        assert_eq!(Geometry::for_window(720, 1280, Some(MAX_DENSITY)).tablet_side_px(), Some(2400));
        assert_eq!(Geometry::for_window(720, 1280, Some(160)).tablet_side_px(), Some(600));
    }

    #[test]
    fn exported_values_round_trip_through_for_window() {
        let g = Geometry::for_window(1024, 768, None);
        let exported = g.export();
        let parsed: Vec<i32> = exported
            .iter()
            .map(|(_, v)| v.parse().expect("numeric"))
            .collect();
        let back = Geometry::for_window(parsed[0] as u32, parsed[1] as u32, Some(parsed[2]));
        assert_eq!(back, g);
    }
}
