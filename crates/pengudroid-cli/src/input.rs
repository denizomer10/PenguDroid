//! `pengudroid input-test` — prove the native input path works.
//!
//! Creates a virtual multitouch panel through `/dev/uinput`, injects a tap and a
//! swipe, and reports the device node the kernel published. This is the
//! "real native Linux call" check: no Android, no compositor, no toolkit.

use pengudroid_input::{keymap, uinput, VirtualTouch};

pub fn test(args: &[String]) -> i32 {
    let width: u32 = args
        .iter()
        .position(|a| a == "--size")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .map(|(w, h): (u32, u32)| w.max(h))
        .unwrap_or(1280);
    let height = width;
    println!("input: creating a {width}x{height} virtual touch panel");

    let mut touch = match VirtualTouch::create("pengudroid-touch", width, height) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("input: cannot create the device: {e}");
            eprintln!("input: /dev/uinput must be writable (add the user to the `input` group)");
            return 1;
        }
    };
    match touch.node() {
        Some(node) => println!("input: device node = {}", node.display()),
        None => println!("input: device created, but the kernel published no event node yet"),
    }

    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    if let Err(e) = touch.tap(cx, cy) {
        eprintln!("input: tap failed: {e}");
        return 1;
    }
    println!("input: tap at {cx},{cy} injected");
    if let Err(e) = touch.swipe(cx, cy, cx + 200.0, cy, 10) {
        eprintln!("input: swipe failed: {e}");
        return 1;
    }
    println!("input: swipe injected");

    // Show what a keymap for this panel would look like, so the format is
    // discoverable from the CLI.
    let sample = keymap::Keymap::parse(&format!(
        "W = hold {cx} {cy}\nSPACE = tap {cx} {}\n",
        cy + 100.0
    ));
    match sample {
        Ok(map) => {
            println!("input: sample keymap ({} binding(s)):", map.bindings.len());
            for line in map.render().lines() {
                println!("  {line}");
            }
        }
        Err(e) => eprintln!("input: sample keymap failed to parse: {e}"),
    }
    println!(
        "input: keymaps live in {}",
        keymap::keymap_path("<package>").display()
    );
    let _ = uinput::keyboards();
    0
}
