//! `pengudroid-daemon`: the always-on broker.
//!
//! Responsibilities (see `docs/ARCHITECTURE.md`):
//! * accept control connections from the CLI, guests, and the Bionic shim;
//! * broker userspace binder transactions (kernel binder when available);
//! * allocate ashmem (memfd) regions and pass their fds to peers;
//! * translate guest surfaces to Wayland toplevels;
//! * hold the audio ring and the Android property store.
//!
//! It is deliberately small and dependency-light so it can live in RAM
//! indefinitely alongside the user's session.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use pengudroid_daemon::{graphics, log, server};
use pengudroid_proto::default_socket_path;
use pengudroid_socket::{Connection, Listener};

use pengudroid_daemon::server::Daemon;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Args {
    socket: PathBuf,
    enable_wayland: bool,
    daemonize: bool,
    wayland_selftest: bool,
    dmabuf_selftest: bool,
}

impl Args {
    fn parse() -> Result<Args, String> {
        let mut args = Args {
            socket: default_socket_path(),
            enable_wayland: true,
            daemonize: false,
            wayland_selftest: false,
            dmabuf_selftest: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--socket" => {
                    let p = it.next().ok_or("--socket needs a PATH")?;
                    args.socket = PathBuf::from(p);
                }
                "--no-wayland" => args.enable_wayland = false,
                "--daemonize" => args.daemonize = true,
                "--wayland-selftest" => args.wayland_selftest = true,
                "--dmabuf-selftest" => args.dmabuf_selftest = true,
                "-v" | "--verbose" => log::set_verbose(true),
                "--version" | "-V" => {
                    println!("pengudroid-daemon {VERSION}");
                    std::process::exit(0);
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(args)
    }
}

fn print_help() {
    println!(
        "pengudroid-daemon {VERSION}\n\
         \n\
         USAGE:\n    pengudroid-daemon [OPTIONS]\n\
         \n\
         OPTIONS:\n\
         \x20   --socket <PATH>       control socket (default: $XDG_RUNTIME_DIR/pengudroid/daemon.sock)\n\
         \x20   --no-wayland          run without the Wayland graphics broker\n\
         \x20   --daemonize           fork into the background\n\
         \x20   --wayland-selftest    connect to Wayland, report, and exit\n\
         \x20   --dmabuf-selftest     render on the GPU and present via linux-dmabuf\n\
         \x20   -v, --verbose         verbose logging\n\
         \x20   -V, --version         print version\n\
         \x20   -h, --help            print this help"
    );
}

fn main() {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    if args.wayland_selftest {
        run_wayland_selftest();
        return;
    }
    if args.dmabuf_selftest {
        run_dmabuf_selftest();
        return;
    }

    install_signal_handlers();

    // Refuse to clobber a live daemon's socket.
    if let Ok(conn) = Connection::connect(&args.socket) {
        if probe(&conn) {
            eprintln!("pengudroid-daemon: already running on {}", args.socket.display());
            std::process::exit(0);
        }
    }

    if args.daemonize {
        if let Err(e) = daemonize() {
            eprintln!("error: daemonize failed: {e}");
            std::process::exit(1);
        }
    }

    let props_path = args
        .socket
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("pengudroid.properties");

    let t0 = Instant::now();
    let daemon = Arc::new(Daemon::new(args.socket.clone(), props_path, args.enable_wayland));
    let listener = match Listener::bind(&args.socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot bind {}: {e}", args.socket.display());
            std::process::exit(1);
        }
    };
    pengudroid_daemon::info!(
        "daemon {} starting; control socket bound in {} ms",
        VERSION,
        t0.elapsed().as_millis()
    );

    if let Err(e) = daemon.run(listener) {
        eprintln!("error: daemon loop failed: {e}");
        std::process::exit(1);
    }
}

/// Connect as a client and do a Ping/Pong to confirm a live daemon.
fn probe(conn: &Connection) -> bool {
    use pengudroid_proto::{Message, Role, PROTO_VERSION};
    if conn
        .send_message(
            &Message::Hello { version: PROTO_VERSION, role: Role::Cli, pid: std::process::id() as i32 },
            &[],
        )
        .is_err()
    {
        return false;
    }
    matches!(conn.recv_message(), Ok((Message::Welcome { .. }, _)))
}

fn run_wayland_selftest() {
    println!("pengudroid-daemon {VERSION} — Wayland self-test");
    match graphics::Graphics::connect() {
        Some(mut g) => {
            g.refresh();
            println!("OK: connected to the Wayland compositor and bound required globals");
            println!(
                "dmabuf: {} ({} formats advertised)",
                if g.dmabuf_available() { "available" } else { "unavailable" },
                g.dmabuf_format_count()
            );
        }
        None => {
            println!("FAIL: no usable Wayland session (set WAYLAND_DISPLAY)");
            std::process::exit(1);
        }
    }
}

/// Render on the host GPU, export the result as a dmabuf, and hand it to the
/// compositor via `zwp_linux_dmabuf_v1` — the zero-copy replacement for the
/// `wl_shm` presentation path.
fn run_dmabuf_selftest() {
    println!("pengudroid-daemon {VERSION} — dmabuf self-test");
    let mut g = match graphics::Graphics::connect() {
        Some(g) => g,
        None => {
            println!("FAIL: no usable Wayland session");
            std::process::exit(1);
        }
    };
    g.refresh();
    println!(
        "compositor dmabuf: {} ({} formats)",
        if g.dmabuf_available() { "available" } else { "unavailable" },
        g.dmabuf_format_count()
    );
    if !g.dmabuf_available() {
        println!("FAIL: compositor does not advertise zwp_linux_dmabuf_v1 (v3+)");
        std::process::exit(1);
    }

    let mut off = match pengudroid_hostgpu::Offscreen::new(256, 256) {
        Ok(o) => o,
        Err(e) => {
            println!("FAIL: cannot create GPU context: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = off.render(0.1, 0.2, 0.9, 1.0) {
        println!("FAIL: GPU render failed: {e}");
        std::process::exit(1);
    }
    let d = match off.export_dmabuf() {
        Ok(d) => d,
        Err(e) => {
            println!("FAIL: dmabuf export failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "exported dmabuf: {}x{} stride {} fourcc {:#010x} modifier {:#x}",
        d.width, d.height, d.stride, d.fourcc, d.modifier
    );

    match g.dmabuf_selftest(d.width, d.height, d.fd, d.stride, d.fourcc, d.modifier, d.offset) {
        Ok(()) => println!("OK: compositor accepted the dmabuf-backed wl_buffer"),
        Err(e) => {
            println!("FAIL: {e}");
            std::process::exit(1);
        }
    }

    // Negative check: a plain memfd is not a dma-buf, so the compositor must
    // reject it. If this wrongly reports success, our failure detection is not
    // observing `zwp_linux_buffer_params_v1.failed` and the positive result
    // above would be untrustworthy.
    let memfd = unsafe { libc::memfd_create(c"pengudroid-not-a-dmabuf".as_ptr(), libc::MFD_CLOEXEC) };
    if memfd >= 0 {
        use std::os::fd::FromRawFd;
        let bad = unsafe { std::os::fd::OwnedFd::from_raw_fd(memfd) };
        let rejected = g
            .dmabuf_selftest(d.width, d.height, bad, d.stride, d.fourcc, 0, 0)
            .is_err();
        if rejected {
            println!("negative check: a plain memfd is rejected (failure detection works)");
        } else {
            println!("WARNING: a plain memfd was accepted; failure detection may be broken");
        }
    }
}

extern "C" fn on_signal(_sig: libc::c_int) {
    // Only async-signal-safe work here: set a flag the accept loop polls.
    server::SHUTDOWN.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() {
    // SAFETY: `on_signal` has the correct `extern "C"` signature for a handler.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        // A guest closing the pipe must not kill the daemon with SIGPIPE.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// Classic double-fork daemonization with stdio redirected to `/dev/null`.
fn daemonize() -> std::io::Result<()> {
    // SAFETY: standard POSIX daemonization sequence.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid > 0 {
            libc::_exit(0);
        }
        if libc::setsid() < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pid2 = libc::fork();
        if pid2 < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid2 > 0 {
            libc::_exit(0);
        }
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
    }
    Ok(())
}
