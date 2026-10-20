//! `pengudroid`: the user-facing command.
//!
//! ```text
//! pengudroid run app.apk      launch a guest that owns a Wayland window
//! pengudroid ps               list running guests (real host processes)
//! pengudroid info app.apk     print manifest details
//! pengudroid getprop NAME     read an Android system property
//! pengudroid setprop N V      write an Android system property
//! pengudroid status / stop    inspect or stop the daemon
//! pengudroid daemon           start the daemon if it is not running
//! ```

mod apk;
mod audio;
mod bundle;
mod client;
mod deps;
mod doctor;
mod gpu;
mod guest;
mod isolate;
mod jni;
mod launcher;
mod nativelib;
mod native;
mod runner;
mod sysroot;
mod telephony;

use pengudroid_proto::{default_socket_path, Message, Role, PROTO_VERSION};

use crate::client::Client;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    // Restore the default SIGPIPE disposition: Rust ignores it, which turns a
    // closed stdout (e.g. `pengudroid ... | head`) into a panic. Behaving like a
    // normal Unix tool and dying on SIGPIPE is the right thing for a CLI.
    // SAFETY: resetting a signal disposition to the default.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    std::process::exit(real_main());
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let Some(cmd) = args.get(1).map(String::as_str) else {
        print_help();
        return 2;
    };
    match cmd {
        "run" => cmd_run(&args[2..]),
        "launch" => launcher::launch(&args[2..]),
        "extract" => cmd_extract(&args[2..]),
        "demo" => cmd_demo(&args[2..]),
        "start" => cmd_start(&args[2..]),
        "ps" => cmd_ps(),
        "info" | "inspect" => cmd_info(&args[2..]),
        "deps" => deps::run(&args[2..]),
        "doctor" => doctor::run(&args[2..]),
        "sysroot" => sysroot::sysroot(&args[2..]),
        "art-test" => sysroot::art_test(&args[2..]),
        "__art" => sysroot::art_main(&args[2..]),
        "gpu-info" => gpu::info(),
        "gpu-test" => gpu::test(),
        "audio-test" => audio::test(&args[2..]),
        "libpath" => nativelib::libpath(&args[2..]),
        "de-bionic" => nativelib::de_bionic(&args[2..]),
        "nativelib" => nativelib::nativelib(&args[2..]),
        "native" => native::native(&args[2..]),
        "__native" => native::native_main(&args[2..]),
        "__dlopen" => nativelib::dlopen_helper(&args[2..]),
        "getprop" => cmd_getprop(&args[2..]),
        "setprop" => cmd_setprop(&args[2..]),
        "services" => cmd_services(),
        "telephony" => telephony::run(&args[2..]),
        "daemon" => cmd_daemon(&args[2..]),
        "stop" => cmd_stop(),
        "status" => cmd_status(),
        "__guest" => guest::guest_main(&args[2..]),
        "-V" | "--version" => {
            println!("pengudroid {VERSION}");
            0
        }
        "-h" | "--help" => {
            print_help();
            0
        }
        other => {
            eprintln!("pengudroid: unknown command '{other}'");
            print_help();
            2
        }
    }
}

fn cmd_start(args: &[String]) -> i32 {
    let demo = args.iter().any(|a| a == "--demo");
    let wait = args.iter().any(|a| a == "--wait");
    if !demo && args.first().map(|a| a.starts_with('-')).unwrap_or(true) {
        eprintln!("usage: pengudroid start <app.apk> [--component C] [--wait] [-- ARGS]");
        eprintln!("       pengudroid start --demo [--wait]");
        return 2;
    }
    let apk = args.first().cloned().unwrap_or_default();
    let mut component: Option<String> = None;
    let mut extra: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--component" => {
                i += 1;
                match args.get(i) {
                    Some(c) => component = Some(c.clone()),
                    None => {
                        eprintln!("--component needs a value");
                        return 2;
                    }
                }
            }
            "--wait" | "--demo" => {}
            "--" => {
                extra.extend_from_slice(&args[i + 1..]);
                break;
            }
            other => extra.push(other.to_string()),
        }
        i += 1;
    }
    let result = if demo {
        runner::start_demo_detached(&extra)
    } else {
        runner::start_detached(&apk, component.as_deref(), &extra)
    };
    match result {
        Ok(pid) => {
            println!("started guest: pid {pid} (detached; `pengudroid ps` lists it)");
            if !wait {
                return 0;
            }
            match runner::wait_detached(pid) {
                Ok(how) => {
                    println!("guest exited: {how}");
                    0
                }
                Err(e) => fail(&e),
            }
        }
        Err(e) => fail(&e),
    }
}

fn cmd_demo(args: &[String]) -> i32 {
    let extra: Vec<String> = match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].to_vec(),
        None => Vec::new(),
    };
    match runner::run_demo(&extra) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            1
        }
    }
}

fn cmd_run(args: &[String]) -> i32 {
    let Some(apk) = args.first() else {
        eprintln!("usage: pengudroid run <app.apk> [--component C] [-- <args>]");
        return 2;
    };
    let mut component: Option<String> = None;
    let mut extra: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--component" => {
                i += 1;
                match args.get(i) {
                    Some(c) => component = Some(c.clone()),
                    None => {
                        eprintln!("--component needs a value");
                        return 2;
                    }
                }
            }
            "--" => {
                extra.extend_from_slice(&args[i + 1..]);
                break;
            }
            other => extra.push(other.to_string()),
        }
        i += 1;
    }
    match runner::run(apk, component.as_deref(), &extra) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            1
        }
    }
}

fn cmd_ps() -> i32 {
    let client = match connect() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    match client.request(&Message::ListProcs) {
        Ok((Message::ProcList { procs }, _)) => {
            if procs.is_empty() {
                println!("no guest processes running");
            } else {
                println!("{:<8} {:<28} {:<8} {}", "PID", "PACKAGE", "SURFACE", "COMPONENT");
                for p in procs {
                    println!("{:<8} {:<28} {:<8} {}", p.pid, p.package, p.surface_id, p.component);
                }
            }
            0
        }
        Ok((other, _)) => fail(&format!("unexpected reply: {other:?}")),
        Err(e) => fail(&e),
    }
}

fn cmd_info(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: pengudroid info <app.apk|.apkm|.xapk|.apks>");
        return 2;
    };
    let p = std::path::Path::new(path);
    if bundle::is_bundle(p) {
        return info_bundle(p);
    }
    match apk::read_apk(path) {
        Ok(info) => {
            println!("{info:#?}");
            if let Ok(entries) = apk::apk_entries(path) {
                println!("archive entries: {}", entries.len());
            }
            0
        }
        Err(e) => fail(&e),
    }
}

fn info_bundle(path: &std::path::Path) -> i32 {
    let b = match bundle::Bundle::open(path) {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };
    let info = match b.info() {
        Ok(i) => i,
        Err(e) => return fail(&e),
    };
    println!("bundle:       {} ({})", path.display(), b.kind.name());
    println!("base apk:     {}", b.base);
    println!("{info:#?}");
    println!("splits:       {}", b.splits.len());
    for s in &b.splits {
        match &s.abi {
            Some(abi) => println!("  {:<40} abi {abi}", s.name),
            None => println!("  {}", s.name),
        }
    }
    if !b.abis.is_empty() {
        println!("abis:         {}", b.abis.join(", "));
    }
    if !b.obb.is_empty() {
        println!("obb files:    {}", b.obb.len());
        for (name, size) in &b.obb {
            println!("  {:<40} {} MB", name, size / 1_048_576);
        }
    }
    0
}

/// `pengudroid extract <file> [--dest DIR] [--abi A]`
///
/// Unpacks an APK or any bundle flavour: the base APK, the splits (filtered to
/// one ABI when asked), and the OBB data in its `Android/obb/<pkg>/` layout.
fn cmd_extract(args: &[String]) -> i32 {
    let mut src: Option<String> = None;
    let mut dest: Option<String> = None;
    let mut abi: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dest" | "-d" => dest = it.next().cloned(),
            "--abi" => abi = it.next().cloned(),
            other if other.starts_with('-') => {
                eprintln!("pengudroid extract: unknown option {other}");
                return 2;
            }
            other => src = Some(other.to_string()),
        }
    }
    let Some(src) = src else {
        eprintln!("usage: pengudroid extract <app.apk|.apkm|.xapk|.apks> [--dest DIR] [--abi ABI]");
        return 2;
    };
    let path = std::path::Path::new(&src);
    let out = dest
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            path.file_stem()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("extracted"))
        });

    let b = match bundle::Bundle::open(path) {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };
    let written = match b.extract_to(&out, abi.as_deref()) {
        Ok(w) => w,
        Err(e) => return fail(&e),
    };
    println!(
        "extracted {} {} member(s) from {} to {}",
        written.len(),
        b.kind.name(),
        path.display(),
        out.display()
    );
    for p in &written {
        println!("  {}", p.display());
    }
    if !b.abis.is_empty() && abi.is_none() {
        println!(
            "note: every ABI split was written; pass --abi <{}> to keep one",
            b.abis.join("|")
        );
    }
    0
}

fn cmd_getprop(args: &[String]) -> i32 {
    let client = match connect() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };

    // `getprop` with no argument lists every property, like Android's tool.
    if args.is_empty() {
        return match client.request(&Message::ListProps) {
            Ok((Message::PropList { pairs, .. }, _)) => {
                for (name, value) in pairs {
                    println!("[{name}]: [{value}]");
                }
                0
            }
            Ok((Message::Error { message, .. }, _)) => fail(&message),
            Ok((other, _)) => fail(&format!("unexpected reply: {other:?}")),
            Err(e) => fail(&e),
        };
    }

    let name = args[0].clone();
    match client.request(&Message::GetProp { name: name.clone() }) {
        Ok((Message::PropValue { value, .. }, _)) => match value {
            Some(v) => println!("[{name}]: [{v}]"),
            None => println!("[{name}]: [<unset>]"),
        },
        Ok((other, _)) => return fail(&format!("unexpected reply: {other:?}")),
        Err(e) => return fail(&e),
    }
    0
}

/// `pengudroid services`: every service the broker can route transactions to.
fn cmd_services() -> i32 {
    let client = match connect() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    match client.request(&Message::ListServices) {
        Ok((Message::ServiceList { services }, _)) => {
            println!("{:<8} {:<28} {}", "HANDLE", "NAME", "INTERFACE");
            for (name, iface, handle) in services {
                println!("{handle:<8} {name:<28} {iface}");
            }
            0
        }
        Ok((Message::Error { message, .. }, _)) => fail(&message),
        Ok((other, _)) => fail(&format!("unexpected reply: {other:?}")),
        Err(e) => fail(&e),
    }
}

fn cmd_setprop(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: pengudroid setprop <name> <value>");
        return 2;
    }
    let client = match connect() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    match client.request(&Message::SetProp { name: args[0].clone(), value: args[1].clone() }) {
        Ok((Message::Ack, _)) => 0,
        Ok((other, _)) => fail(&format!("unexpected reply: {other:?}")),
        Err(e) => fail(&e),
    }
}

fn cmd_daemon(args: &[String]) -> i32 {
    let socket = default_socket_path();
    if args.iter().any(|a| a == "--foreground") {
        // Exec the daemon in the foreground.
        match runner::find_daemon_binary() {
            Ok(bin) => match std::process::Command::new(bin)
                .arg("--socket")
                .arg(&socket)
                .status()
            {
                Ok(s) => return s.code().unwrap_or(0),
                Err(e) => return fail(&format!("cannot start daemon: {e}")),
            },
            Err(e) => return fail(&e),
        }
    }
    match runner::ensure_daemon(&socket) {
        Ok(()) => {
            println!("daemon running on {}", socket.display());
            0
        }
        Err(e) => fail(&e),
    }
}

fn cmd_stop() -> i32 {
    match client::stop() {
        Ok(true) => {
            println!("daemon stopped");
            0
        }
        Ok(false) => {
            println!("daemon did not acknowledge shutdown");
            1
        }
        Err(e) => fail(&e),
    }
}

fn cmd_status() -> i32 {
    let socket = default_socket_path();
    let client = match Client::connect_to(&socket).and_then(|c| {
        c.handshake(Role::Cli)?;
        Ok(c)
    }) {
        Ok(c) => c,
        Err(_) => {
            // A live daemon always answers the handshake; explain the two cases
            // differently so a stale socket file does not look like a bug.
            let stale = socket.exists();
            println!("daemon: not running (socket {})", socket.display());
            if stale {
                println!("        a stale socket file is present; `pengudroid daemon` replaces it");
            }
            return 1;
        }
    };

    match client.request(&Message::Ping) {
        Ok((Message::Pong, _)) => {}
        _ => return fail("daemon is up but unresponsive"),
    }

    let guests = match client.request(&Message::ListProcs) {
        Ok((Message::ProcList { procs }, _)) => procs,
        _ => Vec::new(),
    };
    let services = match client.request(&Message::ListServices) {
        Ok((Message::ServiceList { services }, _)) => services.len(),
        _ => 0,
    };
    let props = match client.request(&Message::ListProps) {
        Ok((Message::PropList { pairs, .. }, _)) => pairs.len(),
        _ => 0,
    };

    println!("daemon: running (socket {})", socket.display());
    println!("protocol: v{PROTO_VERSION}");
    println!("guests:   {}", guests.len());
    println!("services: {services} ({props} properties)");
    for p in guests {
        println!("          pid {} {} {}", p.pid, p.package, p.component);
    }

    0
}

fn connect() -> Result<Client, String> {
    let socket = default_socket_path();
    let c = Client::connect_to(&socket)?;
    c.handshake(Role::Cli)?;
    Ok(c)
}

fn fail(msg: &str) -> i32 {
    eprintln!("pengudroid: {msg}");
    1
}

fn print_help() {
    println!(
        "pengudroid {VERSION} — run Android apps as native Linux processes\n\
         \n\
         USAGE:\n    pengudroid <COMMAND> [ARGS]\n\
         \n\
         COMMANDS:\n\
         \x20   run <app.apk> [--component C] [-- ARGS]    launch a guest window\n\
         \x20   launch <app.apk> [--backend auto|native|art]\n\
         \x20                                              Proton-style launcher (prefix + backend)\n\
         \x20   extract <app.apkm|.xapk|.apks|.apk> [--dest DIR] [--abi A]\n\
         \x20                                              unpack a bundle: base + splits + OBB\n\
         \x20   demo                                        launch the built-in demo window\n\
         \x20   start <app.apk> [--wait]                   let the daemon fork the guest\n\
         \x20   start --demo [--wait]                       same, for the built-in demo\n\
         \x20   ps                                          list running guests\n\
         \x20   info <app.apk>                              show manifest details\n\
         \x20   deps <lib.so>                               bionic import coverage\n\
         \x20   doctor <app.apk>                            can this APK run? what is missing?\n\
         \x20   sysroot <dir>                               inspect an extracted Android system tree\n\
         \x20   art-test --sysroot <dir>                    try JNI_CreateJavaVM against it\n\
         \x20     [--classpath <apk|jar>] [--find-class <C>] [--main <C>]\n\
         \x20     [--jni-lib <so>] [--register-natives <so>]\n\
         \x20   gpu-info                                    report host EGL/GLES/GPU capabilities\n\
         \x20   gpu-test                                    render offscreen on the GPU + export a dmabuf\n\
         \x20   audio-test [--seconds N] [--tone]           play silence (or a tone) through PipeWire\n\
         \x20   nativelib <lib.so> [--call SYM] [--lib-dir D]  de-bionic + dlopen (+ call) a .so\n\
         \x20   nativelib --apk <app.apk> [--abi <abi>]      load an APK's own native libraries\n\
         \x20   native <app.apk> [--seconds N]              run a NativeActivity app (no ART)\n\
         \x20   libpath                                     print the bionic-compat LD_LIBRARY_PATH dir\n\
         \x20   de-bionic <in.so> <out.so>                  strip Android symbol versioning\n\
         \x20   getprop [NAME]                              read a property (no NAME: list all)\n\
         \x20   setprop <NAME> <VALUE>                      write a system property\n\
         \x20   services                                    list the broker's services\n\
         \x20   telephony                                   transact with the phone service and decode replies\n\
         \x20   status | stop                               manage the daemon\n\
         \x20   daemon [--foreground]                       start the daemon\n\
         \n\
         OPTIONS:\n\
         \x20   -V, --version    print version\n\
         \x20   -h, --help       print this help"
    );
}
