//! PenguDroid's desktop shell. One shared state owns app metadata and children;
//! blocking installation and shutdown work stays off Slint's event loop.
mod registry;
mod runner;

use registry::Registry;
use slint::{ModelRc, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

slint::include_modules!();

struct State {
    registry: Registry,
    running: HashMap<String, runner::RunningApp>,
    stopping: HashSet<String>,
    events: std::sync::mpsc::Receiver<WorkerEvent>,
    sender: std::sync::mpsc::Sender<WorkerEvent>,
}

enum WorkerEvent {
    Installed(Result<Vec<Result<registry::Entry, String>>, String>),
    Stopped { package: String, uninstall: bool, result: Result<(), String> },
}

type Shared = Rc<RefCell<State>>;

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let (sender, events) = std::sync::mpsc::channel();
    let state = Rc::new(RefCell::new(State {
        registry: Registry::load(), running: HashMap::new(), stopping: HashSet::new(), events, sender,
    }));
    refresh(&ui, &state.borrow());
    let editor = Rc::new(RefCell::new(None::<KeymapWindow>));

    // Installation (including manifest inspection) is performed on a worker.
    let initial: Vec<_> = std::env::args().skip(1).map(std::path::PathBuf::from).collect();
    if !initial.is_empty() { install(&ui, Rc::clone(&state), Some(initial)); }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        ui.on_install_requested(move || {
            if let Some(ui) = weak.upgrade() { install(&ui, Rc::clone(&state), None); }
        });
    }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        ui.on_selection_changed(move |index| {
            if let Some(ui) = weak.upgrade() { select(&ui, &state.borrow(), index); }
        });
    }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        ui.on_launch_requested(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let mut state = state.borrow_mut();
            let Some(entry) = state.registry.get(index as usize).cloned() else { return };
            if state.running.contains_key(&entry.package) || state.stopping.contains(&entry.package) { return; }
            let fps = state.registry.app_setting(&entry.package, "fps_meter", false);
            match runner::RunningApp::launch(&entry, fps) {
                Ok(child) => {
                    state.registry.set_running(&entry.package, Some(child.id()));
                    state.running.insert(entry.package.clone(), child);
                    ui.set_status(format!("{} is starting…", entry.label).into());
                }
                Err(e) => ui.set_status(format!("Launch failed: {e}").into()),
            }
            refresh(&ui, &state);
        });
    }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        ui.on_stop_requested(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let entry = state.borrow().registry.get(index as usize).cloned();
            if let Some(entry) = entry { stop(&ui, Rc::clone(&state), entry.package, false); }
        });
    }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        ui.on_uninstall_requested(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let entry = state.borrow().registry.get(index as usize).cloned();
            if let Some(entry) = entry { stop(&ui, Rc::clone(&state), entry.package, true); }
        });
    }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        ui.on_keymapper_toggled(move |on| {
            let Some(ui) = weak.upgrade() else { return };
            let mut state = state.borrow_mut();
            let entry = state.registry.get(ui.get_selected() as usize).cloned();
            if let Some(entry) = entry {
                let result = state.registry.set_app_setting(&entry.package, "keymapper", on);
                ui.set_status(match result {
                    Ok(()) => format!("{}: controls profile {}. Runtime mapping is not connected yet.", entry.label, if on { "enabled" } else { "disabled" }).into(),
                    Err(e) => format!("Cannot save preference: {e}").into(),
                });
                select(&ui, &state, ui.get_selected());
            }
        });
    }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        ui.on_fps_meter_toggled(move |on| {
            let Some(ui) = weak.upgrade() else { return };
            let mut state = state.borrow_mut();
            let entry = state.registry.get(ui.get_selected() as usize).cloned();
            if let Some(entry) = entry {
                let result = state.registry.set_app_setting(&entry.package, "fps_meter", on);
                ui.set_status(match result {
                    Ok(()) => "FPS preference saved. Applies on next launch.".into(),
                    Err(e) => format!("Cannot save preference: {e}").into(),
                });
                select(&ui, &state, ui.get_selected());
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_confirm_close_no(move || { if let Some(ui) = weak.upgrade() { ui.set_confirm_close(false); } });
    }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        let editor = Rc::clone(&editor);
        ui.on_confirm_close_yes(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_confirm_close(false);
            if let Some(editor) = editor.borrow().as_ref() { let _ = editor.hide(); }
            ui.set_status("Closing apps…".into());
            let packages: Vec<_> = state.borrow().running.keys().cloned().collect();
            for package in packages { stop(&ui, Rc::clone(&state), package, false); }
            ui.set_quit_when_stopped(true);
        });
    }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        let editor = Rc::clone(&editor);
        ui.window().on_close_requested(move || {
            let Some(ui) = weak.upgrade() else { return slint::CloseRequestResponse::HideWindow };
            if state.borrow().registry.running_count() == 0 {
                if let Some(editor) = editor.borrow().as_ref() { let _ = editor.hide(); }
                return slint::CloseRequestResponse::HideWindow;
            }
            ui.set_confirm_close(true);
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state); let editor = Rc::clone(&editor);
        ui.on_edit_keymap(move || {
            let Some(ui) = weak.upgrade() else { return };
            let entry = state.borrow().registry.get(ui.get_selected() as usize).cloned();
            let Some(entry) = entry else { return };
            let Ok(window) = KeymapWindow::new() else { return };
            window.set_app_name(entry.label.into());
            window.set_profile(std::fs::read_to_string(pengudroid_input::keymap::keymap_path(&entry.package)).unwrap_or_default().into());
            let weak = window.as_weak();
            window.on_save_profile(move |text| {
                let Some(window) = weak.upgrade() else { return };
                let result = pengudroid_input::keymap::Keymap::parse(&text)
                    .and_then(|map| pengudroid_input::keymap::save(&entry.package, &map).map_err(|e| e.to_string()));
                window.set_message(match result {
                    Ok(_) => "Controls saved for this app.".into(), Err(e) => format!("Cannot save controls: {e}").into(),
                });
            });
            let _ = window.show();
            *editor.borrow_mut() = Some(window);
        });
    }
    // Owned children are polled and reaped; every callback observes this same
    // state. A guest window close therefore clears its running row automatically.
    let timer = slint::Timer::default();
    {
        let weak = ui.as_weak(); let state = Rc::clone(&state);
        timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(250), move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut state = state.borrow_mut();
            let mut changed = false;
            while let Ok(event) = state.events.try_recv() {
                changed = true;
                match event {
                    WorkerEvent::Installed(result) => {
                        match result {
                            Ok(results) => {
                                let mut messages = Vec::new();
                                for result in results { match result {
                                    Ok(entry) => { messages.push(format!("Installed {}", entry.label)); state.registry.merge_installed(entry); }
                                    Err(e) => messages.push(format!("Install failed: {e}")),
                                } }
                                ui.set_status(if messages.is_empty() { "Install cancelled".into() } else { messages.join(" · ").into() });
                            }
                            Err(e) => ui.set_status(e.into()),
                        }
                        ui.set_installing(false);
                    }
                    WorkerEvent::Stopped { package, uninstall, result } => {
                        state.stopping.remove(&package);
                        state.registry.set_running(&package, None);
                        let result = result.and_then(|_| if uninstall { state.registry.uninstall(&package) } else { Ok(()) });
                        ui.set_status(match result {
                            Ok(()) => if uninstall { "App removed".into() } else { "App stopped".into() }, Err(e) => e.into(),
                        });
                    }
                }
            }
            let exited: Vec<_> = state.running.iter_mut().filter_map(|(package, child)| {
                match child.try_wait() {
                    Ok(Some(status)) => Some((package.clone(), format!("App exited: {status}"))),
                    Err(e) => Some((package.clone(), format!("Cannot read app state: {e}"))),
                    Ok(None) => None,
                }
            }).collect();
            for (package, status) in exited {
                changed = true;
                state.running.remove(&package);
                state.registry.set_running(&package, None);
                ui.set_status(status.into());
            }
            if changed { refresh(&ui, &state); }
            if ui.get_quit_when_stopped() && state.registry.running_count() == 0 { let _ = ui.hide(); }
        });
    }
    // Optional native-window snapshot for reviewing the launcher on a real
    // desktop. It includes the GUI only, not other applications on the screen.
    if let Some(path) = std::env::var_os("PENGUDROID_GUI_SCREENSHOT") {
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_secs(3), move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Ok(pixels) = ui.window().take_snapshot() {
                let mut ppm = format!("P6\n{} {}\n255\n", pixels.width(), pixels.height()).into_bytes();
                for pixel in pixels.as_bytes().chunks_exact(4) { ppm.extend_from_slice(&pixel[..3]); }
                if let Err(e) = std::fs::write(path, ppm) { eprintln!("GUI snapshot failed: {e}"); }
            }
        });
    }
    ui.run()
}

fn install(ui: &MainWindow, state: Shared, paths: Option<Vec<std::path::PathBuf>>) {
    if ui.get_installing() { return; }
    ui.set_installing(true);
    ui.set_status("Choose an Android package to install…".into());
    let sender = state.borrow().sender.clone();
    std::thread::spawn(move || {
        let result = paths.or_else(Registry::choose_apks).ok_or_else(|| "No file chooser found. Install zenity, kdialog or yad.".to_string())
            .map(|paths| paths.into_iter().map(|path| {
                let mut registry = Registry::load();
                registry.install(&path)
            }).collect::<Vec<_>>());
        let _ = sender.send(WorkerEvent::Installed(result));
    });
}

fn stop(ui: &MainWindow, state: Shared, package: String, uninstall: bool) {
    if uninstall && ui.get_installing() { return; }
    let (child, sender) = {
        let mut state = state.borrow_mut();
        if state.stopping.contains(&package) { return; }
        state.stopping.insert(package.clone());
        (state.running.remove(&package), state.sender.clone())
    };
    ui.set_status("Stopping app…".into());
    refresh(ui, &state.borrow());
    std::thread::spawn(move || {
        let result = child.map_or(Ok(()), |child| child.stop());
        let _ = sender.send(WorkerEvent::Stopped { package, uninstall, result });
    });
}

fn select(ui: &MainWindow, state: &State, index: i32) {
    if let Some(entry) = state.registry.get(index as usize) {
        ui.set_keymapper_on(state.registry.app_setting(&entry.package, "keymapper", false));
        ui.set_fps_meter_on(state.registry.app_setting(&entry.package, "fps_meter", false));
    }
}

fn refresh(ui: &MainWindow, state: &State) {
    let entries: Vec<AppEntry> = state.registry.apps().iter().map(|a| AppEntry {
        package: a.package.clone().into(), label: a.label.clone().into(), activity: a.activity.clone().into(),
        running: a.running, stopping: state.stopping.contains(&a.package),
        icon: slint::Image::load_from_path(&registry::icon_path(&a.package)).unwrap_or_default(),
        has_icon: registry::icon_path(&a.package).is_file(),
    }).collect();
    ui.set_apps(ModelRc::new(VecModel::from(entries)));
    ui.set_running_count(state.registry.running_count() as i32);
    if ui.get_selected() < 0 && !state.registry.apps().is_empty() { ui.set_selected(0); }
    select(ui, state, ui.get_selected());
}
