//! Built-in keymaps for games whose touch layout is known.
//!
//! A preset is only a starting point: it is used when the user has no profile
//! of their own for the package, and `pengudroid keymap-preset <package>`
//! prints it so it can be saved and edited. Layouts are stored as fractions of
//! the display, because the same game is run at 1280x720 and at 1920x1080 and
//! its HUD scales with the screen.

use crate::keymap::{keycode, Action, Binding, Dir, Keymap};

/// The action a preset key performs, in display fractions.
#[derive(Debug, Clone, Copy)]
enum Slot {
    Tap(f32, f32),
    Hold(f32, f32),
    /// Centre x, centre y, radius as a fraction of the display *height*.
    Stick(f32, f32, f32, Dir),
    /// Mouse-look around a centre; radius as a fraction of the height.
    Look(f32, f32, f32),
}

/// Genshin Impact's default mobile HUD (16:9, landscape).
///
/// The positions are read off the stock control layout, not measured inside a
/// running game under PenguDroid, and the game lets players move its buttons —
/// treat this as approximate and adjust it in the controls editor.
const GENSHIN: &[(&str, Slot)] = &[
    // Movement pad, lower left. It floats: it anchors where the finger lands.
    ("W", Slot::Stick(0.155, 0.745, 0.14, Dir::Up)),
    ("A", Slot::Stick(0.155, 0.745, 0.14, Dir::Left)),
    ("S", Slot::Stick(0.155, 0.745, 0.14, Dir::Down)),
    ("D", Slot::Stick(0.155, 0.745, 0.14, Dir::Right)),
    // Combat cluster, lower right. `hold`, so a long press charges the attack
    // and holds the elemental skill the way a finger would.
    //
    // Left Alt toggles mouse-look, the way it frees the cursor on PC: the
    // mouse then turns the camera (a drag on the empty right half of the
    // screen) and the left button attacks. With mouse-look off the cursor is
    // a finger again, for menus.
    ("LEFTALT", Slot::Look(0.62, 0.36, 0.22)),
    ("MOUSE_LEFT", Slot::Hold(0.865, 0.815)),
    ("MOUSE_RIGHT", Slot::Hold(0.935, 0.875)),
    ("E", Slot::Hold(0.765, 0.875)),
    ("Q", Slot::Tap(0.672, 0.895)),
    ("SPACE", Slot::Tap(0.935, 0.68)),
    ("LEFTSHIFT", Slot::Hold(0.935, 0.875)),
    // Pick up / talk prompt, centre right.
    ("F", Slot::Tap(0.66, 0.5)),
    // Party slots, right edge.
    ("1", Slot::Tap(0.925, 0.315)),
    ("2", Slot::Tap(0.925, 0.405)),
    ("3", Slot::Tap(0.925, 0.495)),
    ("4", Slot::Tap(0.925, 0.585)),
    // Minimap, upper left.
    ("M", Slot::Tap(0.085, 0.13)),
];

/// Packages that ship the Genshin Impact client.
const GENSHIN_PACKAGES: &[&str] = &[
    "com.miHoYo.GenshinImpact",
    "com.miHoYo.Yuanshen",
    "com.miHoYo.ys.bilibili",
];

/// Every package that has a preset.
pub fn packages() -> Vec<&'static str> {
    GENSHIN_PACKAGES.to_vec()
}

/// The built-in keymap for `package` on a `width` x `height` display, if any.
pub fn for_package(package: &str, width: f32, height: f32) -> Option<Keymap> {
    let layout = if GENSHIN_PACKAGES.contains(&package) {
        GENSHIN
    } else {
        return None;
    };
    if !(width.is_finite() && height.is_finite()) || width < 1.0 || height < 1.0 {
        return None;
    }
    let px = |fx: f32, fy: f32| ((fx * width).round(), (fy * height).round());
    let bindings = layout
        .iter()
        .map(|(key, slot)| {
            let code = keycode(key).expect("preset key names are valid");
            let action = match *slot {
                Slot::Tap(fx, fy) => {
                    let (x, y) = px(fx, fy);
                    Action::Tap { x, y }
                }
                Slot::Hold(fx, fy) => {
                    let (x, y) = px(fx, fy);
                    Action::Hold { x, y }
                }
                Slot::Look(fx, fy, fr) => {
                    let (cx, cy) = px(fx, fy);
                    Action::Look {
                        cx,
                        cy,
                        radius: (fr * height).round().max(1.0),
                        sensitivity: 1.0,
                    }
                }
                Slot::Stick(fx, fy, fr, dir) => {
                    let (cx, cy) = px(fx, fy);
                    Action::Stick { cx, cy, radius: (fr * height).round().max(1.0), dir }
                }
            };
            Binding { code, action }
        })
        .collect();
    Some(Keymap { bindings })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genshin_preset_scales_with_the_display() {
        let hd = for_package("com.miHoYo.GenshinImpact", 1280.0, 720.0).expect("preset");
        let fhd = for_package("com.miHoYo.GenshinImpact", 1920.0, 1080.0).expect("preset");
        assert_eq!(hd.bindings.len(), GENSHIN.len());
        let w = keycode("W").unwrap();
        assert_eq!(
            hd.action_for(w),
            Some(&Action::Stick { cx: 198.0, cy: 536.0, radius: 101.0, dir: Dir::Up })
        );
        assert_eq!(
            fhd.action_for(w),
            Some(&Action::Stick { cx: 298.0, cy: 805.0, radius: 151.0, dir: Dir::Up })
        );
    }

    /// WASD must be one joystick: a different centre per key would be four
    /// unrelated fingers.
    #[test]
    fn movement_keys_share_one_stick() {
        let map = for_package("com.miHoYo.Yuanshen", 1280.0, 720.0).expect("preset");
        let centres: Vec<(f32, f32, f32)> = ["W", "A", "S", "D"]
            .iter()
            .map(|k| match map.action_for(keycode(k).unwrap()) {
                Some(Action::Stick { cx, cy, radius, .. }) => (*cx, *cy, *radius),
                other => panic!("{k}: {other:?}"),
            })
            .collect();
        assert!(centres.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn every_preset_stays_on_screen_and_survives_a_round_trip() {
        for package in packages() {
            for (w, h) in [(1280.0f32, 720.0f32), (1920.0, 1080.0), (2400.0, 1080.0)] {
                let map = for_package(package, w, h).expect(package);
                for b in &map.bindings {
                    let (x, y, reach) = match b.action {
                        Action::Tap { x, y } | Action::Hold { x, y } => (x, y, 0.0),
                        Action::Stick { cx, cy, radius, .. }
                        | Action::Look { cx, cy, radius, .. } => (cx, cy, radius),
                        Action::Swipe { x0, y0, .. } => (x0, y0, 0.0),
                    };
                    assert!(x - reach >= 0.0 && x + reach <= w, "{package} {b:?}");
                    assert!(y - reach >= 0.0 && y + reach <= h, "{package} {b:?}");
                }
                // What `keymap-preset` prints must load back unchanged.
                let parsed = Keymap::parse(&map.render()).expect("render parses");
                assert_eq!(parsed.bindings, map.bindings);
            }
        }
    }

    #[test]
    fn unknown_packages_and_bad_sizes_have_no_preset() {
        assert!(for_package("org.schabi.newpipe", 1280.0, 720.0).is_none());
        assert!(for_package("com.miHoYo.GenshinImpact", 0.0, 720.0).is_none());
        assert!(for_package("com.miHoYo.GenshinImpact", f32::NAN, 720.0).is_none());
    }
}
