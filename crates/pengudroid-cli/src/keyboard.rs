//! Physical keyboard state for Android input, independent of JNI.
use std::collections::HashMap;

#[derive(Default)]
pub struct Keyboard {
    held: HashMap<u32, (i64, i32)>,
    caps_lock: bool,
}

pub struct Key {
    pub code: i32,
    pub action: i32,
    pub down_time: i64,
    pub repeat: i32,
    pub meta: i32,
}

impl Keyboard {
    pub fn event(&mut self, scan: u32, pressed: bool, now: i64) -> Option<Key> {
        let code = android_keycode(scan)?;
        let (down_time, repeat) = if pressed {
            let entry = self.held.entry(scan).or_insert((now, -1));
            entry.1 = entry.1.saturating_add(1);
            if scan == 58 && entry.1 == 0 {
                self.caps_lock = !self.caps_lock;
            }
            *entry
        } else {
            self.held.remove(&scan).unwrap_or((now, 0))
        };
        // Android meta flags include both the side-specific and aggregate bits.
        let mut meta = if self.caps_lock { 0x100000 } else { 0 };
        for (scan, bits) in [
            (42, 0x41), (54, 0x81), // shift
            (56, 0x12), (100, 0x22), // alt
            (29, 0x3000), (97, 0x5000), // ctrl
            (125, 0x30000), (126, 0x50000), // meta
        ] {
            if self.held.contains_key(&scan) {
                meta |= bits;
            }
        }
        Some(Key { code, action: if pressed { 0 } else { 1 }, down_time, repeat, meta })
    }
}

/// Map a Linux input keycode (`KEY_*`) to an Android `KeyEvent` key code.
///
/// Physical US-layout keys, editing/navigation keys and modifiers. The host
/// compositor keymap and IME text composition are not translated here.
/// Unknown keys return `None` rather than dispatching a wrong character.
pub fn android_keycode(linux: u32) -> Option<i32> {
    Some(match linux {
        // Desktop keyboards have no dedicated Back key. Escape is the
        // conventional stand-in (the Android emulator maps it the same way);
        // `KEYCODE_ESCAPE` is not handled by most apps, so offering it would
        // leave the user unable to leave a screen without the mouse.
        1 => 4, // KEY_ESC -> KEYCODE_BACK
        2 => 8, // KEY_1
        3 => 9,
        4 => 10,
        5 => 11,
        6 => 12,
        7 => 13,
        8 => 14,
        9 => 15,
        10 => 16,
        11 => 7,  // KEY_0
        12 => 69, // KEY_MINUS
        13 => 70, // KEY_EQUAL
        14 => 67, // KEY_BACKSPACE
        15 => 61, // KEY_TAB
        // q w e r t y u i o p
        16 => 45,
        17 => 51,
        18 => 33,
        19 => 46,
        20 => 48,
        21 => 53,
        22 => 49,
        23 => 37,
        24 => 43,
        25 => 44,
        26 => 71, // KEY_LEFTBRACE
        27 => 72, // KEY_RIGHTBRACE
        28 => 66, // KEY_ENTER
        // a s d f g h j k l
        29 => 113, // KEY_LEFTCTRL
        30 => 29,
        31 => 47,
        32 => 32,
        33 => 34,
        34 => 35,
        35 => 36,
        36 => 38,
        37 => 39,
        38 => 40,
        39 => 74, // KEY_SEMICOLON
        40 => 75, // KEY_APOSTROPHE
        41 => 68, // KEY_GRAVE
        42 => 59, // KEY_LEFTSHIFT
        43 => 73, // KEY_BACKSLASH
        // z x c v b n m
        44 => 54,
        45 => 52,
        46 => 31,
        47 => 50,
        48 => 30,
        49 => 42,
        50 => 41,
        51 => 55,  // KEY_COMMA
        52 => 56,  // KEY_DOT
        53 => 76,  // KEY_SLASH
        54 => 60, // KEY_RIGHTSHIFT
        56 => 57, // KEY_LEFTALT
        58 => 115, // KEY_CAPSLOCK
        97 => 114, // KEY_RIGHTCTRL
        100 => 58, // KEY_RIGHTALT
        102 => 122, // KEY_HOME
        104 => 92, // KEY_PAGEUP
        107 => 123, // KEY_END
        109 => 93, // KEY_PAGEDOWN
        110 => 124, // KEY_INSERT
        111 => 112, // KEY_DELETE
        125 => 117, // KEY_LEFTMETA
        126 => 118, // KEY_RIGHTMETA
        158 => 4, // KEY_BACK -> KEYCODE_BACK (laptop/media keyboards)
        57 => 62,  // KEY_SPACE
        103 => 19, // KEY_UP
        105 => 21, // KEY_LEFT
        106 => 22, // KEY_RIGHT
        108 => 20, // KEY_DOWN
        _ => return None,
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_and_back_keys_reach_android_back() {
        // Without this a desktop user cannot leave an Android screen from the
        // keyboard: `KEYCODE_ESCAPE` is not a substitute for `KEYCODE_BACK`.
        assert_eq!(android_keycode(1), Some(4));
        assert_eq!(android_keycode(158), Some(4));
        // The rest of the table stays untouched.
        assert_eq!(android_keycode(2), Some(8));
        assert_eq!(android_keycode(28), Some(66));
    }

    #[test]
    fn shift_chord_retains_other_side_until_released() {
        let mut keyboard = Keyboard::default();
        keyboard.event(42, true, 10);
        keyboard.event(54, true, 11);
        assert_eq!(keyboard.event(30, true, 12).unwrap().meta, 0xc1);
        assert_eq!(keyboard.event(42, false, 13).unwrap().meta, 0x81);
        assert_eq!(keyboard.event(30, false, 14).unwrap().meta, 0x81);
        assert_eq!(keyboard.event(54, false, 15).unwrap().meta, 0);
    }

    #[test]
    fn repeat_and_release_keep_original_down_time() {
        let mut keyboard = Keyboard::default();
        assert_eq!(keyboard.event(30, true, 10).unwrap().repeat, 0);
        let repeat = keyboard.event(30, true, 20).unwrap();
        assert_eq!((repeat.down_time, repeat.repeat), (10, 1));
        let up = keyboard.event(30, false, 30).unwrap();
        assert_eq!((up.down_time, up.action), (10, 1));
        assert_eq!(keyboard.event(30, true, 40).unwrap().down_time, 40);
    }

    #[test]
    fn caps_lock_toggles_once_per_press() {
        let mut keyboard = Keyboard::default();
        keyboard.event(58, true, 1);
        keyboard.event(58, true, 2);
        keyboard.event(58, false, 3);
        assert_eq!(keyboard.event(30, true, 4).unwrap().meta, 0x100000);
        keyboard.event(58, true, 5);
        assert_eq!(keyboard.event(30, false, 6).unwrap().meta, 0);
    }

    #[test]
    fn shortcuts_and_editing_keys_reach_android() {
        let mut keyboard = Keyboard::default();
        keyboard.event(29, true, 1);
        let select_all = keyboard.event(30, true, 2).unwrap();
        assert_eq!((select_all.code, select_all.meta), (29, 0x3000));
        assert_eq!(android_keycode(111), Some(112)); // forward delete
        assert_eq!(android_keycode(102), Some(122)); // home
        assert!(keyboard.event(999, true, 3).is_none());
    }
}

/// Android 13's native KeyCharacterMap parcel, including the JNI wrapper's
/// device id and presence flag. Native read/writeToParcel validate this layout
/// again at runtime. See frameworks/native/libs/input/KeyCharacterMap.cpp.
/// This is a US physical layout; compositor XKB layouts are a separate layer.
pub fn virtual_character_map_parcel() -> Vec<u8> {
    let mut keys: Vec<(i32, char, char)> = (0..26)
        .map(|i| (29 + i, (b'a' + i as u8) as char, (b'A' + i as u8) as char))
        .collect();
    for (i, (plain, shifted)) in "0123456789".chars().zip(")!@#$%^&*(".chars()).enumerate() {
        keys.push((7 + i as i32, plain, shifted));
    }
    keys.extend([
        (55, ',', '<'), (56, '.', '>'), (62, ' ', ' '), (61, '\t', '\t'),
        (66, '\n', '\n'), (68, '`', '~'), (69, '-', '_'), (70, '=', '+'),
        (71, '[', '{'), (72, ']', '}'), (73, '\\', '|'), (74, ';', ':'),
        (75, '\'', '"'), (76, '/', '?'),
    ]);
    keys.sort_by_key(|k| k.0);
    let mut words = vec![-1i32, 1, 0, 4, 0, keys.len() as i32];
    // [device=-1][present][empty C string padded to 4][FULL][overlay=false][keys]
    for (code, plain, shifted) in keys {
        let number = if plain.is_ascii_digit() { plain as i32 } else { 0 };
        words.extend([code, shifted as i32, number]);
        if plain.is_ascii_lowercase() {
            // Most specific first, matching native getKeyBehavior's first hit.
            for (meta, ch) in [(0x100001, plain), (0x100000, shifted), (1, shifted), (0, plain)] {
                words.extend([1, meta, ch as i32, 0, 0]);
            }
        } else {
            for (meta, ch) in [(1, shifted), (0, plain)] {
                words.extend([1, meta, ch as i32, 0, 0]);
            }
        }
        words.push(0); // end of behavior list
    }
    words.extend([0, 0]); // no scan-code or HID usage overlays
    words.into_iter().flat_map(i32::to_le_bytes).collect()
}
