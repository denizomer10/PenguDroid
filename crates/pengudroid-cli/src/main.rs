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
mod client;
mod deps;
mod gpu;
mod guest;
mod nativelib;
mod runner;

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
        "ps" => cmd_ps(),
        "info" | "inspect" => cmd_info(&args[2..]),
        "deps" => deps::run(&args[2..]),
        "gpu-info" => gpu::info(),
        "gpu-test" => gpu::test(),
        "libpath" => nativelib::libpath(&args[2..]),
        "de-bionic" => nativelib::de_bionic(&args[2..]),
        "nativelib" => nativelib::nativelib(&args[2..]),
        "__dlopen" => nativelib::dlopen_helper(&args[2..]),
        "getprop" => cmd_getprop(&args[2..]),
        "setprop" => cmd_setprop(&args[2..]),
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
        eprintln!("usage: pengudroid info <app.apk>");
        return 2;
    };
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

fn cmd_getprop(args: &[String]) -> i32 {
    let client = match connect() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let names: Vec<String> = if args.is_empty() {
        // No enumeration message in the protocol; query a useful default set.
        [
            "ro.build.version.sdk",
            "ro.build.version.release",
            "ro.product.model",
            "ro.debuggable",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    } else {
        vec![args[0].clone()]
    };
    for name in names {
        match client.request(&Message::GetProp { name: name.clone() }) {
            Ok((Message::PropValue { value, .. }, _)) => match value {
                Some(v) => println!("[{name}]: [{v}]"),
                None => println!("[{name}]: [<unset>]"),
            },
            Ok((other, _)) => return fail(&format!("unexpected reply: {other:?}")),
            Err(e) => return fail(&e),
        }
    }
    0
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
    match Client::connect_to(&socket).and_then(|c| {
        c.handshake(Role::Cli)?;
        Ok(c)
    }) {
        Ok(c) => match c.request(&Message::Ping) {
            Ok((Message::Pong, _)) => {
                println!("daemon: running (socket {})", socket.display());
                println!("protocol: v{PROTO_VERSION}");
                0
            }
            _ => fail("daemon is up but unresponsive"),
        },
        Err(_) => {
            println!("daemon: not running (socket {})", socket.display());
            1
        }
    }
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
         \x20   run <app.apk> [--component C] [-- ARGS]   launch a guest window\n\
         \x20   ps                                          list running guests\n\
         \x20   info <app.apk>                              show manifest details\n\
         \x20   deps <lib.so>                               bionic import coverage\n\
         \x20   gpu-info                                    report host EGL/GLES/GPU capabilities\n\
         \x20   gpu-test                                    render offscreen on the GPU + export a dmabuf\n\
         \x20   nativelib <lib.so> [--call SYM] [--lib-dir D]  de-bionic + dlopen (+ call) a native .so\n\
         \x20   nativelib --apk <app.apk> [--abi <abi>]      load an APK's own native libraries\n\
         \x20   libpath                                     print the bionic-compat LD_LIBRARY_PATH dir\n\
         \x20   de-bionic <in.so> <out.so>                  strip Android symbol versioning\n\
         \x20   getprop [NAME]                              read a system property\n\
         \x20   setprop <NAME> <VALUE>                      write a system property\n\
         \x20   status | stop                               manage the daemon\n\
         \x20   daemon [--foreground]                       start the daemon\n\
         \n\
         OPTIONS:\n\
         \x20   -V, --version    print version\n\
         \x20   -h, --help       print this help"
    );
}
