//! The keymapper: physical keys in, touch gestures out.
//!
//! One thread per keyboard device. A `Hold` binding owns a finger for as long as
//! the key is down, which is what a virtual joystick needs; `Tap` and `Swipe`
//! fire on the press edge and ignore the release.
//!
//! The mapper never touches the guest directly: it writes to a [`VirtualTouch`],
//! so whatever consumes input — the guest's own input queue, a compositor, a
//! game's SDL loop — sees a normal touchscreen.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::keymap::{Action, Keymap};
use crate::uinput::{EventReader, VirtualTouch};

/// A running keymapper.
pub struct KeyMapper {
    stop: Arc<AtomicBool>,
    /// Bindings can be replaced while the mapper runs (the GUI edits them).
    bindings: Arc<Mutex<BTreeMap<u16, Action>>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl KeyMapper {
    /// Start mapping `devices` through `touch`.
    ///
    /// `touch` is shared because several keyboards may be mapped at once and
    /// they all drive the same panel.
    pub fn start(
        devices: Vec<PathBuf>,
        touch: Arc<Mutex<VirtualTouch>>,
        keymap: &Keymap,
    ) -> KeyMapper {
        let stop = Arc::new(AtomicBool::new(false));
        let bindings = Arc::new(Mutex::new(crate::keymap::index(keymap)));
        let mut threads = Vec::new();
        for path in devices {
            let stop = Arc::clone(&stop);
            let bindings = Arc::clone(&bindings);
            let touch = Arc::clone(&touch);
            threads.push(std::thread::spawn(move || {
                let Ok(mut reader) = EventReader::open(&path) else {
                    return;
                };
                // Which finger each held key owns, so a release frees the right
                // one. `Hold` is the only action that spans two events.
                let mut held: HashMap<u16, ()> = HashMap::new();
                while !stop.load(Ordering::Relaxed) {
                    let Ok((code, pressed)) = reader.read_key() else {
                        break;
                    };
                    let action = bindings
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&code)
                        .cloned();
                    let Some(action) = action else {
                        continue;
                    };
                    let mut panel = touch.lock().unwrap_or_else(|e| e.into_inner());
                    match action {
                        Action::Tap { x, y } => {
                            if pressed {
                                let _ = panel.tap(x, y);
                            }
                        }
                        Action::Hold { x, y } => {
                            if pressed {
                                if held.insert(code, ()).is_none() {
                                    let _ = panel.down(x, y);
                                }
                            } else if held.remove(&code).is_some() {
                                let _ = panel.up();
                            }
                        }
                        Action::Swipe {
                            x0,
                            y0,
                            x1,
                            y1,
                            steps,
                        } => {
                            if pressed {
                                let _ = panel.swipe(x0, y0, x1, y1, steps);
                            }
                        }
                    }
                }
            }));
        }
        KeyMapper {
            stop,
            bindings,
            threads,
        }
    }

    /// Replace the bindings without restarting the threads.
    pub fn set_keymap(&self, keymap: &Keymap) {
        *self.bindings.lock().unwrap_or_else(|e| e.into_inner()) = crate::keymap::index(keymap);
    }

    /// Stop every thread and wait for them.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_can_be_replaced_while_running() {
        let keymap = Keymap::parse("W = tap 1 2\n").expect("parse");
        let bindings = Arc::new(Mutex::new(crate::keymap::index(&keymap)));
        assert!(bindings.lock().unwrap().contains_key(&17));
        let next = Keymap::parse("A = tap 3 4\n").expect("parse");
        *bindings.lock().unwrap() = crate::keymap::index(&next);
        assert!(!bindings.lock().unwrap().contains_key(&17));
        assert!(bindings.lock().unwrap().contains_key(&30));
    }
}
