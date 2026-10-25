//! PenguDroid's desktop shell.
//!
//! A launcher for the Android apps PenguDroid can run: install an APK, start it,
//! stop it, remove it, and toggle the in-game helpers. The window is Slint; the
//! work happens in [`registry`] (what is installed) and [`runner`]
//! (start/stop), which delegate to the `pengudroid` CLI so the GUI and the
//! command line cannot drift apart.

mod registry;
mod runner;

use pengudroid_input::keymap;
use pengudroid_input::mapper::KeyMapper;
use pengudroid_input::uinput::{keyboards, VirtualTouch};
use registry::Registry;
use slint::{ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// The keymapper for the app that is currently running.
///
/// One at a time: the panel is a single virtual touchscreen, so two mappers
/// would fight over the same finger. Starting a second app replaces the first.
struct ActiveMapper {
    mapper: KeyMapper,
    /// Kept alive for the mapper's threads.
    _touch: Arc<Mutex<VirtualTouch>>,
}

impl ActiveMapper {
    fn stop(self) {
        self.mapper.stop();
    }
}

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let mut registry = Registry::load();
    let mapper: Rc<RefCell<Option<ActiveMapper>>> = Rc::new(RefCell::new(None));

    // Anything passed on the command line is installed first, so
    // `pengudroid-gui game.apk` works as a one-shot installer (and as the
    // drop target for a file manager's "open with").
    for arg in std::env::args().skip(1) {
        if arg.ends_with(".apk") || arg.ends_with(".apkm") || arg.ends_with(".xapk") {
            match registry.install(std::path::Path::new(&arg)) {
                Ok(entry) => ui.set_status(format!("Installed {}", entry.label).into()),
                Err(e) => ui.set_status(format!("Install failed: {e}").into()),
            }
        }
    }

    refresh(&ui, &registry);
    ui.set_keymapper_on(registry.setting("keymapper", true));
    ui.set_fps_meter_on(registry.setting("fps_meter", false));

    // ---------------------------------------------------------------- install
    //
    // The chooser runs on its own thread: `zenity`/`kdialog` block until the user
    // answers, and blocking the Slint event loop would freeze the window (no
    // repaint, no input) for as long as the dialog is open. The result comes back
    // through `invoke_from_event_loop`, which is the only thread-safe way to touch
    // the UI.
    {
        let ui_weak = ui.as_weak();
        let fallback = registry.clone();
        ui.on_install_requested(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_status("Choosing a package…".into());
            let ui_weak = ui.as_weak();
            let fallback = fallback.clone();
            std::thread::spawn(move || {
                let picked = Registry::choose_apks().or_else(|| {
                    // No native chooser installed: fall back to the directory
                    // scan so `+` still does something useful.
                    fallback.pick_apk().map(|p| vec![p])
                });
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_weak.upgrade() else { return };
                    let mut registry = Registry::load();
                    match picked {
                        None => ui.set_status(
                            "No file chooser found (install zenity, kdialog or yad)".into(),
                        ),
                        Some(paths) if paths.is_empty() => {
                            ui.set_status("Install cancelled".into());
                        }
                        Some(paths) => {
                            let mut installed = Vec::new();
                            let mut errors = Vec::new();
                            for path in paths {
                                match registry.install(&path) {
                                    Ok(entry) => installed.push(entry.label),
                                    Err(e) => errors.push(format!("{}: {e}", path.display())),
                                }
                            }
                            let mut status = if installed.is_empty() {
                                String::new()
                            } else {
                                format!("Installed {}", installed.join(", "))
                            };
                            if !errors.is_empty() {
                                if !status.is_empty() {
                                    status.push_str(" — ");
                                }
                                status.push_str(&format!("failed: {}", errors.join("; ")));
                            }
                            ui.set_status(status.into());
                        }
                    }
                    refresh(&ui, &registry);
                });
            });
        });
    }

    // ----------------------------------------------------------------- launch
    {
        let ui_weak = ui.as_weak();
        let mut registry = registry.clone();
        let mapper = Rc::clone(&mapper);
        ui.on_launch_requested(move |index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(entry) = registry.get(index as usize).cloned() else { return };
            match runner::launch(&entry) {
                Ok(pid) => {
                    registry.set_running(&entry.package, Some(pid));
                    let mut status = format!("Started {} (pid {pid})", entry.label);
                    // The keymapper is per app: its bindings are that app's
                    // layout, so it starts with the app and dies with it.
                    if registry.setting("keymapper", true) {
                        if let Some(previous) = mapper.borrow_mut().take() {
                            previous.stop();
                        }
                        match start_keymapper(&entry.package) {
                            Ok(active) => {
                                let bindings = active.mapper.binding_count();
                                *mapper.borrow_mut() = Some(active);
                                status.push_str(&format!(", keymapper on ({bindings} binding(s))"));
                            }
                            Err(e) => status.push_str(&format!(", keymapper off: {e}")),
                        }
                    }
                    ui.set_status(status.into());
                }
                Err(e) => ui.set_status(format!("Launch failed: {e}").into()),
            }
            refresh(&ui, &registry);
        });
    }

    // ------------------------------------------------------------------- stop
    {
        let ui_weak = ui.as_weak();
        let mut registry = registry.clone();
        let mapper = Rc::clone(&mapper);
        ui.on_stop_requested(move |index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(entry) = registry.get(index as usize).cloned() else { return };
            if let Some(active) = mapper.borrow_mut().take() {
                active.stop();
            }
            match runner::stop(&entry) {
                Ok(()) => ui.set_status(format!("Stopped {}", entry.label).into()),
                Err(e) => ui.set_status(format!("Stop failed: {e}").into()),
            }
            registry.set_running(&entry.package, None);
            refresh(&ui, &registry);
        });
    }

    // -------------------------------------------------------------- uninstall
    {
        let ui_weak = ui.as_weak();
        let mut registry = registry.clone();
        let mapper = Rc::clone(&mapper);
        ui.on_uninstall_requested(move |index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(entry) = registry.get(index as usize).cloned() else { return };
            if let Some(active) = mapper.borrow_mut().take() {
                active.stop();
            }
            let _ = runner::stop(&entry);
            match registry.uninstall(&entry.package) {
                Ok(()) => {
                    ui.set_status(format!("Removed {}", entry.label).into());
                    ui.set_selected(-1);
                }
                Err(e) => ui.set_status(format!("Uninstall failed: {e}").into()),
            }
            refresh(&ui, &registry);
        });
    }

    // ---------------------------------------------------------------- settings
    {
        let ui_weak = ui.as_weak();
        let mut registry = registry.clone();
        ui.on_keymapper_toggled(move |on| {
            let Some(ui) = ui_weak.upgrade() else { return };
            registry.set_setting("keymapper", on);
            ui.set_keymapper_on(on);
            ui.set_status(if on { "Keymapper on".into() } else { "Keymapper off".into() });
        });
    }
    {
        let ui_weak = ui.as_weak();
        let mut registry = registry.clone();
        ui.on_fps_meter_toggled(move |on| {
            let Some(ui) = ui_weak.upgrade() else { return };
            registry.set_setting("fps_meter", on);
            ui.set_fps_meter_on(on);
            ui.set_status(if on { "FPS meter on".into() } else { "FPS meter off".into() });
        });
    }

    // ------------------------------------------------------- close confirmation
    {
        let ui_weak = ui.as_weak();
        ui.on_confirm_close_no(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_confirm_close(false);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let mut registry = registry.clone();
        let mapper = Rc::clone(&mapper);
        ui.on_confirm_close_yes(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if let Some(active) = mapper.borrow_mut().take() {
                active.stop();
            }
            for entry in registry.apps().to_vec() {
                if entry.running {
                    let _ = runner::stop(&entry);
                    registry.set_running(&entry.package, None);
                }
            }
            let _ = ui.hide();
        });
    }

    // Closing the window must not silently kill running apps: ask first.
    {
        let ui_weak = ui.as_weak();
        let registry = registry.clone();
        ui.window().on_close_requested(move || {
            let Some(ui) = ui_weak.upgrade() else {
                return slint::CloseRequestResponse::HideWindow;
            };
            let running = registry.running_count();
            if running == 0 {
                return slint::CloseRequestResponse::HideWindow;
            }
            ui.set_running_count(running as i32);
            ui.set_confirm_close(true);
            slint::CloseRequestResponse::KeepWindowShown
        });
    }

    ui.run()
}

/// Create the virtual panel and start mapping this package's keys onto it.
fn start_keymapper(package: &str) -> Result<ActiveMapper, String> {
    let touch = VirtualTouch::create("pengudroid-touch", 720, 1280)
        .map_err(|e| format!("cannot create the touch panel: {e}"))?;
    let touch = Arc::new(Mutex::new(touch));
    let map = keymap::load(package);
    let devices = keyboards();
    if devices.is_empty() {
        return Err("no readable keyboard device".into());
    }
    let mapper = KeyMapper::start(devices, Arc::clone(&touch), &map);
    Ok(ActiveMapper {
        mapper,
        _touch: touch,
    })
}

/// Push the registry into the UI model.
fn refresh(ui: &MainWindow, registry: &Registry) {
    let entries: Vec<AppEntry> = registry
        .apps()
        .iter()
        .map(|a| AppEntry {
            package: a.package.clone().into(),
            label: a.label.clone().into(),
            activity: a.activity.clone().into(),
            running: a.running,
        })
        .collect();
    ui.set_apps(ModelRc::new(VecModel::from(entries)));
    ui.set_running_count(registry.running_count() as i32);
}
