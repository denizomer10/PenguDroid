//! Native Linux input for PenguDroid.
//!
//! Two halves, both talking to the kernel rather than to a toolkit:
//!
//! * [`uinput`] creates virtual devices and injects real input events. This is
//!   how a touch reaches a game that reads input the way a device does — its own
//!   `AInputQueue`, an SDL2/Unity/Unreal loop, or a compositor — instead of
//!   relying on the app's Java view tree.
//! * [`keymap`] stores per-package key bindings and turns them into gestures, so
//!   a keyboard can drive a game that only understands a touchscreen.
//!
//! [`mapper`] joins the two: it reads a physical keyboard through `evdev` and
//! replays each bound key as a touch on the virtual panel.

pub mod keymap;
pub mod mapper;
pub mod uinput;

pub use keymap::{Action, Binding, Keymap};
pub use mapper::KeyMapper;
pub use uinput::{EventReader, InputEvent, VirtualKeyboard, VirtualTouch};
