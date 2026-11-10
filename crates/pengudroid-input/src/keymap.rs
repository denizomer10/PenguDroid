//! Per-app key mappings: a physical key becomes a touch gesture.
//!
//! Games that expect a touchscreen are unplayable with a keyboard, so the
//! keymapper turns keys into the gestures the game already understands. The
//! mapping is stored per package, in a small line-based format:
//!
//! ```text
//! # Genshin Impact
//! W       = hold 640 900
//! Space   = tap  660 1150
//! Shift   = swipe 400 800 700 800 12
//! A       = stick 230 520 110 left
//! MOUSE_RIGHT = tap 1110 590
//! ```
//!
//! `stick` is a virtual joystick: every key bound to the same centre and
//! radius shares one finger, which rests on the centre and is dragged toward
//! the sum of the held directions. That is what a game's floating movement pad
//! expects, and it is why WASD needs its own action instead of four `hold`s —
//! four separate fingers would be four unrelated touches.
//!
//! The format is deliberately not JSON: the project has no serialisation
//! dependency, and a line per binding is easier to hand-edit and to diff. Keys
//! are Linux key names (`KEY_W`, `W`, `SPACE`, ...) and the numbers are in the
//! touch panel's own units.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What a key does while it is held.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Touch and release immediately.
    Tap { x: f32, y: f32 },
    /// Touch on press, release on key-up — a virtual joystick button.
    Hold { x: f32, y: f32 },
    /// Drag from one point to another.
    Swipe {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        steps: u32,
    },
    /// Toggle mouse-look: the pointer is locked and its motion drags a finger
    /// around `(cx, cy)`, which is how a touch game turns its camera.
    ///
    /// The finger is lifted and put back on the centre whenever it strays more
    /// than `radius` away, so the camera can turn forever inside a small area.
    /// `sensitivity` is guest pixels per unit of mouse motion.
    Look {
        cx: f32,
        cy: f32,
        radius: f32,
        sensitivity: f32,
    },
    /// One direction of a virtual joystick centred on `(cx, cy)`.
    ///
    /// Keys that share a centre and radius drive the same finger.
    Stick {
        cx: f32,
        cy: f32,
        radius: f32,
        dir: Dir,
    },
}

/// A joystick direction, in screen space (`Up` is toward smaller `y`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    /// The unit vector this direction contributes.
    pub fn vector(self) -> (f32, f32) {
        match self {
            Dir::Up => (0.0, -1.0),
            Dir::Down => (0.0, 1.0),
            Dir::Left => (-1.0, 0.0),
            Dir::Right => (1.0, 0.0),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Dir::Up => "up",
            Dir::Down => "down",
            Dir::Left => "left",
            Dir::Right => "right",
        }
    }

    fn parse(text: &str) -> Option<Dir> {
        match text {
            "up" => Some(Dir::Up),
            "down" => Some(Dir::Down),
            "left" => Some(Dir::Left),
            "right" => Some(Dir::Right),
            _ => None,
        }
    }
}

/// One key binding.
#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    /// Linux keycode (`KEY_*`).
    pub code: u16,
    pub action: Action,
}

/// A package's keymap.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    pub bindings: Vec<Binding>,
}

impl Keymap {
    /// Parse the line format. Unknown lines are reported, not silently dropped.
    pub fn parse(text: &str) -> Result<Keymap, String> {
        let mut bindings = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, rest)) = line.split_once('=') else {
                return Err(format!("line {}: expected `KEY = action …`", index + 1));
            };
            let code = keycode(key.trim())
                .ok_or_else(|| format!("line {}: unknown key `{}`", index + 1, key.trim()))?;
            let action = parse_action(rest.trim())
                .map_err(|e| format!("line {}: {e}", index + 1))?;
            bindings.push(Binding { code, action });
        }
        Ok(Keymap { bindings })
    }

    /// Render back to the line format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for b in &self.bindings {
            let key = keyname(b.code);
            let action = match b.action {
                Action::Tap { x, y } => format!("tap {x} {y}"),
                Action::Hold { x, y } => format!("hold {x} {y}"),
                Action::Swipe {
                    x0,
                    y0,
                    x1,
                    y1,
                    steps,
                } => format!("swipe {x0} {y0} {x1} {y1} {steps}"),
                Action::Stick { cx, cy, radius, dir } => {
                    format!("stick {cx} {cy} {radius} {}", dir.name())
                }
                Action::Look { cx, cy, radius, sensitivity } => {
                    format!("look {cx} {cy} {radius} {sensitivity}")
                }
            };
            out.push_str(&format!("{key} = {action}\n"));
        }
        out
    }

    pub fn action_for(&self, code: u16) -> Option<&Action> {
        self.bindings.iter().find(|b| b.code == code).map(|b| &b.action)
    }
}

fn parse_action(text: &str) -> Result<Action, String> {
    let mut parts = text.split_whitespace();
    let kind = parts.next().ok_or("missing action")?;
    let words: Vec<&str> = parts.collect();
    let nums = || -> Result<Vec<f32>, String> {
        words
            .iter()
            .map(|p| {
                let value = p.parse::<f32>().map_err(|_| format!("`{p}` is not a number"))?;
                if !value.is_finite() || value < 0.0 { return Err(format!("`{p}` must be finite and non-negative")); }
                Ok(value)
            })
            .collect()
    };
    match kind {
        "tap" => {
            let n = nums()?;
            match n.as_slice() {
                [x, y] => Ok(Action::Tap { x: *x, y: *y }),
                _ => Err("tap needs x y".into()),
            }
        }
        "hold" => {
            let n = nums()?;
            match n.as_slice() {
                [x, y] => Ok(Action::Hold { x: *x, y: *y }),
                _ => Err("hold needs x y".into()),
            }
        }
        "swipe" => {
            let n = nums()?;
            match n.as_slice() {
                [x0, y0, x1, y1] => Ok(Action::Swipe {
                    x0: *x0,
                    y0: *y0,
                    x1: *x1,
                    y1: *y1,
                    steps: 12,
                }),
                [x0, y0, x1, y1, steps] if *steps >= 1.0 && *steps <= 240.0 && steps.fract() == 0.0 => Ok(Action::Swipe {
                    x0: *x0,
                    y0: *y0,
                    x1: *x1,
                    y1: *y1,
                    steps: steps.max(1.0) as u32,
                }),
                _ => Err("swipe needs x0 y0 x1 y1 [steps]; steps must be an integer from 1 to 240".into()),
            }
        }
        "look" => {
            const USAGE: &str = "look needs cx cy radius [sensitivity]";
            let n = nums()?;
            let (cx, cy, radius, sensitivity) = match n.as_slice() {
                [cx, cy, radius] => (*cx, *cy, *radius, 1.0),
                [cx, cy, radius, sensitivity] => (*cx, *cy, *radius, *sensitivity),
                _ => return Err(USAGE.into()),
            };
            if radius < 1.0 {
                return Err("look radius must be at least 1".into());
            }
            if !(0.01..=100.0).contains(&sensitivity) {
                return Err("look sensitivity must be between 0.01 and 100".into());
            }
            Ok(Action::Look { cx, cy, radius, sensitivity })
        }
        "stick" => {
            const USAGE: &str = "stick needs cx cy radius up|down|left|right";
            let [cx, cy, radius, dir] = words.as_slice() else {
                return Err(USAGE.into());
            };
            let dir = Dir::parse(dir).ok_or(USAGE)?;
            let mut values = [0.0f32; 3];
            for (slot, word) in values.iter_mut().zip([cx, cy, radius]) {
                let value = word
                    .parse::<f32>()
                    .map_err(|_| format!("`{word}` is not a number"))?;
                if !value.is_finite() || value < 0.0 {
                    return Err(format!("`{word}` must be finite and non-negative"));
                }
                *slot = value;
            }
            if values[2] < 1.0 {
                return Err("stick radius must be at least 1".into());
            }
            Ok(Action::Stick { cx: values[0], cy: values[1], radius: values[2], dir })
        }
        other => Err(format!("unknown action `{other}`")),
    }
}

/// `BTN_LEFT`: the primary button. It is the touch pointer; a binding for it
/// takes effect only while mouse-look is on.
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;

/// Linux keycode for a name, accepting both `KEY_W` and `W`.
pub fn keycode(name: &str) -> Option<u16> {
    let upper = name.trim().to_ascii_uppercase();
    let bare = upper.strip_prefix("KEY_").unwrap_or(&upper);
    let code = match bare {
        "ESC" | "ESCAPE" => 1,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "MINUS" => 12,
        "EQUAL" => 13,
        "BACKSPACE" => 14,
        "TAB" => 15,
        "Q" => 16,
        "W" => 17,
        "E" => 18,
        "R" => 19,
        "T" => 20,
        "Y" => 21,
        "U" => 22,
        "I" => 23,
        "O" => 24,
        "P" => 25,
        "LEFTBRACE" => 26,
        "RIGHTBRACE" => 27,
        "ENTER" => 28,
        "LEFTCTRL" | "CTRL" => 29,
        "A" => 30,
        "S" => 31,
        "D" => 32,
        "F" => 33,
        "G" => 34,
        "H" => 35,
        "J" => 36,
        "K" => 37,
        "L" => 38,
        "SEMICOLON" => 39,
        "APOSTROPHE" => 40,
        "GRAVE" => 41,
        "LEFTSHIFT" | "SHIFT" => 42,
        "BACKSLASH" => 43,
        "Z" => 44,
        "X" => 45,
        "C" => 46,
        "V" => 47,
        "B" => 48,
        "N" => 49,
        "M" => 50,
        "COMMA" => 51,
        "DOT" => 52,
        "SLASH" => 53,
        "RIGHTSHIFT" => 54,
        "LEFTALT" | "ALT" => 56,
        "SPACE" => 57,
        "CAPSLOCK" => 58,
        "F1" => 59,
        "F2" => 60,
        "F3" => 61,
        "F4" => 62,
        "F5" => 63,
        "F6" => 64,
        "F7" => 65,
        "F8" => 66,
        "F9" => 67,
        "F10" => 68,
        "F11" => 87,
        "F12" => 88,
        "UP" => 103,
        "LEFT" => 105,
        "RIGHT" => 106,
        "DOWN" => 108,
        "HOME" => 102,
        "END" => 107,
        "PAGEUP" => 104,
        "PAGEDOWN" => 109,
        "INSERT" => 110,
        "DELETE" => 111,
        "RIGHTCTRL" => 97,
        "RIGHTALT" => 100,
        // Mouse buttons other than the left one, which is the touch itself.
        // They share the keycode space (`BTN_RIGHT`, `BTN_MIDDLE`, ...), so a
        // binding can put attack on the right button the way a PC game does.
        // The left button is the touch pointer, so its binding only applies
        // while mouse-look has the pointer locked and there is no cursor to
        // click with.
        "MOUSE_LEFT" | "BTN_LEFT" => BTN_LEFT,
        "MOUSE_RIGHT" | "BTN_RIGHT" => BTN_RIGHT,
        "MOUSE_MIDDLE" | "BTN_MIDDLE" => BTN_MIDDLE,
        "MOUSE_SIDE" | "BTN_SIDE" => BTN_SIDE,
        "MOUSE_EXTRA" | "BTN_EXTRA" => BTN_EXTRA,
        _ => return None,
    };
    Some(code)
}

/// Inverse of [`keycode`], for rendering.
pub fn keyname(code: u16) -> String {
    const NAMES: &[(u16, &str)] = &[
        (1, "ESC"),
        (2, "1"),
        (3, "2"),
        (4, "3"),
        (5, "4"),
        (6, "5"),
        (7, "6"),
        (8, "7"),
        (9, "8"),
        (10, "9"),
        (11, "0"),
        (14, "BACKSPACE"),
        (15, "TAB"),
        (16, "Q"),
        (17, "W"),
        (18, "E"),
        (19, "R"),
        (20, "T"),
        (21, "Y"),
        (22, "U"),
        (23, "I"),
        (24, "O"),
        (25, "P"),
        (28, "ENTER"),
        (29, "LEFTCTRL"),
        (30, "A"),
        (31, "S"),
        (32, "D"),
        (33, "F"),
        (34, "G"),
        (35, "H"),
        (36, "J"),
        (37, "K"),
        (38, "L"),
        (42, "LEFTSHIFT"),
        (44, "Z"),
        (45, "X"),
        (46, "C"),
        (47, "V"),
        (48, "B"),
        (49, "N"),
        (50, "M"),
        (56, "LEFTALT"),
        (57, "SPACE"),
        (59, "F1"),
        (60, "F2"),
        (61, "F3"),
        (62, "F4"),
        (63, "F5"),
        (64, "F6"),
        (65, "F7"),
        (66, "F8"),
        (67, "F9"),
        (68, "F10"),
        (87, "F11"),
        (88, "F12"),
        (102, "HOME"),
        (103, "UP"),
        (104, "PAGEUP"),
        (105, "LEFT"),
        (106, "RIGHT"),
        (107, "END"),
        (108, "DOWN"),
        (109, "PAGEDOWN"),
        (110, "INSERT"),
        (111, "DELETE"),
        (12, "MINUS"),
        (13, "EQUAL"),
        (26, "LEFTBRACE"),
        (27, "RIGHTBRACE"),
        (39, "SEMICOLON"),
        (40, "APOSTROPHE"),
        (41, "GRAVE"),
        (43, "BACKSLASH"),
        (51, "COMMA"),
        (52, "DOT"),
        (53, "SLASH"),
        (54, "RIGHTSHIFT"),
        (58, "CAPSLOCK"),
        (97, "RIGHTCTRL"),
        (100, "RIGHTALT"),
        (BTN_LEFT, "MOUSE_LEFT"),
        (BTN_RIGHT, "MOUSE_RIGHT"),
        (BTN_MIDDLE, "MOUSE_MIDDLE"),
        (BTN_SIDE, "MOUSE_SIDE"),
        (BTN_EXTRA, "MOUSE_EXTRA"),
    ];
    NAMES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| (*n).to_string())
        .unwrap_or_else(|| format!("KEY_{code}"))
}

/// Where keymaps live: `$XDG_CONFIG_HOME/pengudroid/keymaps/<package>.keymap`.
pub fn keymap_path(package: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("pengudroid").join("keymaps").join(format!("{package}.keymap"))
}

/// Load a package's keymap, or an empty one when there is no file yet.
pub fn load(package: &str) -> Keymap {
    std::fs::read_to_string(keymap_path(package))
        .ok()
        .and_then(|text| Keymap::parse(&text).ok())
        .unwrap_or_default()
}

/// Write a package's keymap, creating the directory if needed.
pub fn save(package: &str, keymap: &Keymap) -> std::io::Result<PathBuf> {
    let path = keymap_path(package);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, keymap.render())?;
    Ok(path)
}

/// A keymap's bindings indexed by keycode, for the event loop.
pub fn index(keymap: &Keymap) -> BTreeMap<u16, Action> {
    keymap
        .bindings
        .iter()
        .map(|b| (b.code, b.action.clone()))
        .collect()
}

/// Whether `path` looks like a keymap file.
pub fn is_keymap(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "keymap")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_action() {
        let text = "\
# a comment
W = hold 640 900
SPACE = tap 660 1150
SHIFT = swipe 400 800 700 800 12
";
        let map = Keymap::parse(text).expect("parse");
        assert_eq!(map.bindings.len(), 3);
        assert_eq!(
            map.action_for(17),
            Some(&Action::Hold { x: 640.0, y: 900.0 })
        );
        assert_eq!(
            map.action_for(57),
            Some(&Action::Tap { x: 660.0, y: 1150.0 })
        );
        assert_eq!(
            map.action_for(42),
            Some(&Action::Swipe {
                x0: 400.0,
                y0: 800.0,
                x1: 700.0,
                y1: 800.0,
                steps: 12
            })
        );
    }

    #[test]
    fn round_trips() {
        let text = "W = hold 10 20\nSPACE = tap 30 40\n";
        let map = Keymap::parse(text).expect("parse");
        assert_eq!(map.render(), text);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Keymap::parse("NOPE = tap 1 2").is_err());
        assert!(Keymap::parse("W = tap 1").is_err());
        assert!(Keymap::parse("W tap 1 2").is_err());
        assert!(Keymap::parse("W = jump 1 2").is_err());
        assert!(Keymap::parse("W = tap NaN 2").is_err());
        assert!(Keymap::parse("W = hold -1 2").is_err());
        assert!(Keymap::parse("W = swipe 0 0 1 1 999999").is_err());
        assert!(Keymap::parse("W = swipe 0 0 1 1 1.5").is_err());
        assert!(Keymap::parse("W = stick 1 2 3").is_err());
        assert!(Keymap::parse("W = stick 1 2 3 north").is_err());
        assert!(Keymap::parse("W = stick 1 2 0 up").is_err());
        assert!(Keymap::parse("W = stick 1 2 3 up extra").is_err());
        assert!(Keymap::parse("W = look 1 2").is_err());
        assert!(Keymap::parse("W = look 1 2 0").is_err());
        assert!(Keymap::parse("W = look 1 2 3 0").is_err());
        assert!(Keymap::parse("W = look 1 2 3 4 5").is_err());
    }

    #[test]
    fn stick_parses_and_round_trips() {
        let text = "W = stick 230 520 110 up\nMOUSE_RIGHT = tap 1110 590\n";
        let map = Keymap::parse(text).expect("parse");
        assert_eq!(
            map.action_for(17),
            Some(&Action::Stick { cx: 230.0, cy: 520.0, radius: 110.0, dir: Dir::Up })
        );
        assert_eq!(map.action_for(BTN_RIGHT), Some(&Action::Tap { x: 1110.0, y: 590.0 }));
        assert_eq!(map.render(), text);
    }

    #[test]
    fn look_parses_with_a_default_sensitivity_and_round_trips() {
        let map = Keymap::parse("LEFTALT = look 900 300 200\nMOUSE_LEFT = hold 1 2\n").expect("parse");
        assert_eq!(
            map.action_for(56),
            Some(&Action::Look { cx: 900.0, cy: 300.0, radius: 200.0, sensitivity: 1.0 })
        );
        assert_eq!(map.action_for(BTN_LEFT), Some(&Action::Hold { x: 1.0, y: 2.0 }));
        let rendered = map.render();
        assert_eq!(rendered, "LEFTALT = look 900 300 200 1\nMOUSE_LEFT = hold 1 2\n");
        assert_eq!(Keymap::parse(&rendered).expect("reparse").bindings, map.bindings);
    }

    /// A name `render` cannot write back would make the GUI editor silently
    /// corrupt a profile on save.
    #[test]
    fn every_parsable_key_renders_to_a_parsable_name() {
        for code in 1..=0x114u16 {
            let name = keyname(code);
            if name.starts_with("KEY_") {
                continue;
            }
            assert_eq!(keycode(&name), Some(code), "{name}");
        }
        for name in ["MINUS", "GRAVE", "CAPSLOCK", "RIGHTSHIFT", "MOUSE_MIDDLE", "SLASH"] {
            let code = keycode(name).expect(name);
            assert_eq!(keyname(code), name);
        }
    }

    #[test]
    fn key_names_round_trip() {
        for name in ["W", "A", "SPACE", "LEFTSHIFT", "F12", "UP"] {
            let code = keycode(name).expect(name);
            assert_eq!(keyname(code), name);
        }
        assert_eq!(keycode("KEY_W"), keycode("W"));
    }
}
