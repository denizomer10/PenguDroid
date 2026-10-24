//! PenguDroid's desktop shell.
//!
//! A launcher for the Android apps PenguDroid can run: install an APK, start it,
//! stop it, remove it, and toggle the in-game helpers. The window is Slint; the
//! work happens in [`registry`] (what is installed) and [`runner`]
//! (start/stop), which delegate to the `pengudroid` CLI so the GUI and the
//! command line cannot drift apart.

mod registry;
mod runner;

use registry::Registry;
use slint::{ModelRc, VecModel};

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let mut registry = Registry::load();

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
    {
        let ui_weak = ui.as_weak();
        let mut registry = registry.clone();
        ui.on_install_requested(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            match registry.pick_apk() {
                Some(path) => match registry.install(&path) {
                    Ok(entry) => ui.set_status(format!("Installed {}", entry.label).into()),
                    Err(e) => ui.set_status(format!("Install failed: {e}").into()),
                },
                None => ui.set_status(
                    "No .apk found in ~/Downloads or the current directory".into(),
                ),
            }
            refresh(&ui, &registry);
        });
    }

    // ----------------------------------------------------------------- launch
    {
        let ui_weak = ui.as_weak();
        let mut registry = registry.clone();
        ui.on_launch_requested(move |index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(entry) = registry.get(index as usize).cloned() else { return };
            match runner::launch(&entry) {
                Ok(pid) => {
                    registry.set_running(&entry.package, Some(pid));
                    ui.set_status(format!("Started {} (pid {pid})", entry.label).into());
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
        ui.on_stop_requested(move |index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(entry) = registry.get(index as usize).cloned() else { return };
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
        ui.on_uninstall_requested(move |index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(entry) = registry.get(index as usize).cloned() else { return };
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
        ui.on_confirm_close_yes(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
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
