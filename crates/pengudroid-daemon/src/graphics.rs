//! Graphics broker: translates Android's surface model directly to Wayland.
//!
//! # What this replaces
//!
//! On real Android, an app's pixels travel
//! `EGL/GLES → gralloc GraphicBuffer → BufferQueue → SurfaceFlinger → display`.
//! PenguDroid does **not** run SurfaceFlinger. Instead it collapses that chain:
//! a guest surface becomes a `wl_surface` + `xdg_toplevel`, and its pixels live
//! in an [`AshmemRegion`] (a memfd) that is handed to the guest as an fd and
//! simultaneously wrapped in a `wl_shm` buffer. The compositor reads the same
//! memory the guest writes — no copy through a display server.
//!
//! For buffer exchange with a full GPU guest we would use the
//! `linux-dmabuf`/`zwp_linux_dmabuf_v1` path with host Vulkan allocations (the
//! "Lepton-style" driver pass-through), which is the natural upgrade from
//! `wl_shm` here. The daemon already owns the allocation and the fd, so the
//! swap is localized to [`State::finalize_surface`].
//!
//! # Status
//!
//! Real Wayland client: connects to the compositor, binds `wl_compositor`,
//! `wl_shm`, `xdg_wm_base`, creates toplevels, and presents a placeholder frame.
//! Guest pixel upload into the shared buffer is wired (the guest receives the
//! fd) but the MVP daemon also paints the frame itself so a window is visibly
//! produced end-to-end.

/// Describes a surface back to the control-socket peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceInfo {
    pub id: u32,
    /// Name of the backing ashmem region (also used as the memfd name).
    pub shm_name: String,
    pub stride: u32,
    pub size: u64,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

#[cfg(feature = "wayland")]
mod imp {
    use super::SurfaceInfo;
    use crate::ashmem::AshmemRegion;
    use std::collections::HashMap;
    use std::os::fd::AsFd;

    use pengudroid_proto::abi;
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
        wl_seat, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface,
    };
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols::wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
    };
    use wayland_protocols::xdg::decoration::zv1::client::{
        zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
    };
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    /// A mapped toplevel plus the memory backing it.
    #[allow(dead_code)] // `configured`/`closed` are diagnostics today.
    pub struct Surface {
        pub wl_surface: wl_surface::WlSurface,
        /// `None` for a subsurface: only a toplevel has an `xdg_surface`.
        pub xdg_surface: Option<xdg_surface::XdgSurface>,
        pub xdg_toplevel: Option<xdg_toplevel::XdgToplevel>,
        /// Set when this surface is a child layer of another one.
        pub subsurface: Option<wl_subsurface::WlSubsurface>,
        pub buffer: wl_buffer::WlBuffer,
        /// Kept alive so the memfd is not closed while the compositor may read
        /// it, and so `present` can repaint.
        pub region: AshmemRegion,
        /// When the surface is fed by an imported dma-buf, its fd is kept alive
        /// here (the compositor holds its own reference, but the buffer's memory
        /// must stay valid while attached).
        pub dmabuf_fd: Option<std::os::fd::OwnedFd>,
        pub width: u32,
        pub height: u32,
        pub configured: bool,
        pub closed: bool,
    }

    /// The broker's surface id, carried as a toplevel's user data so events can
    /// name their own window.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct SurfaceId(u32);

    /// A surface waiting for its first `xdg_surface.configure`.
    struct Pending {
        id: u32,
        width: u32,
        height: u32,
        xdg_surface: xdg_surface::XdgSurface,
        wl_surface: wl_surface::WlSurface,
        /// Read through the region's fd on `finalize_surface`.
        region: AshmemRegion,
        stride: u32,
    }

    #[derive(Default)]
    pub struct State {
        pub compositor: Option<wl_compositor::WlCompositor>,
        pub shm: Option<wl_shm::WlShm>,
        pub wm_base: Option<xdg_wm_base::XdgWmBase>,
        /// `zxdg_decoration_manager_v1`, when the compositor offers it.
        ///
        /// Without a decoration object the window has no title bar on XFCE or
        /// KDE (GNOME draws its own), so there is no close or maximise button —
        /// the user can only kill the guest. Asking for server-side decorations
        /// is what makes the window behave like a normal application window.
        pub decoration: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
        /// `zwp_linux_dmabuf_v1`, when the compositor advertises it (v3+, which
        /// carries format modifiers).
        pub dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
        /// `wl_subcompositor`, used for child layers (`SurfaceView` video).
        pub subcompositor: Option<wl_subcompositor::WlSubcompositor>,
        /// How many `format`/`modifier` events the compositor sent (diagnostic).
        pub dmabuf_formats: u32,
        /// The `(fourcc, modifier)` pairs the compositor advertised. v1-v2 send
        /// `format` only (modifier 0); v3+ send `modifier` events, and v4 may
        /// send *only* those, so both are recorded.
        pub dmabuf_supported: Vec<(u32, u64)>,
        /// Result flags for the dmabuf self-test.
        pub dmabuf_created: bool,
        pub dmabuf_failed: bool,
        next_id: u32,
        surfaces: HashMap<u32, Surface>,
        pending: Vec<Pending>,
        /// Toplevel proxies awaiting their surface's first configure.
        toplevels: HashMap<u32, xdg_toplevel::XdgToplevel>,
        /// Decoration proxies, kept alive for the surface's lifetime.
        decorations: HashMap<u32, zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
        /// Frames presented per surface, for the FPS meter:
        /// `(frames since the last update, when that was)`.
        fps: HashMap<u32, (u32, std::time::Instant)>,
        /// Ids of surfaces the compositor asked us to close.
        pub close_requests: Vec<u32>,
        /// The seat, once the compositor advertises one.
        pub seat: Option<wl_seat::WlSeat>,
        pub pointer: Option<wl_pointer::WlPointer>,
        pub keyboard: Option<wl_keyboard::WlKeyboard>,
        /// Surface under the pointer, and the last position in surface pixels.
        pointer_focus: Option<u32>,
        pointer_pos: (f64, f64),
        /// Input waiting to be forwarded to the guest that owns the surface.
        pub input: Vec<(u32, pengudroid_proto::InputKind)>,
    }

    impl State {
        fn alloc_id(&mut self) -> u32 {
            self.next_id += 1;
            self.next_id
        }

        /// Which broker surface a `wl_surface` belongs to.
        fn surface_id_of(&self, s: &wl_surface::WlSurface) -> Option<u32> {
            self.surfaces
                .iter()
                .find(|(_, v)| v.wl_surface.id() == s.id())
                .map(|(id, _)| *id)
        }

        fn push_input(&mut self, event: pengudroid_proto::InputKind) {
            if let Some(id) = self.pointer_focus {
                self.input.push((id, event));
            }
        }

        /// Build the `wl_shm` buffer for a pending surface once the compositor
        /// has configured it.
        ///
        /// The buffer is a read-only view of the surface's fd, which the guest
        /// also maps. Holding it in a `foreign` slot keeps it alive even after
        /// the guest (and this process's own handle) goes away, so the memfd
        /// itself is not released until the compositor has the newest frame.
        fn finalize_surface(&mut self, xdg: &xdg_surface::XdgSurface, qh: &QueueHandle<State>) {
            let Some(idx) = self.pending.iter().position(|p| p.xdg_surface.id() == xdg.id()) else {
                return;
            };
            let p = self.pending.remove(idx);
            let Some(shm) = self.shm.as_ref() else { return };

            // Paint a visible placeholder so the window is not blank: a purple
            // gradient band that makes it obvious this is PenguDroid.
            if let Ok(mut map) = p.region.map_mut() {
                let bytes = map.as_mut_slice();
                let w = p.width.max(1);
                for y in 0..p.height {
                    let t = (y * 255 / p.height.max(1)) as u8;
                    for x in 0..w {
                        let o = (y * p.stride + x * 4) as usize;
                        if o + 3 < bytes.len() {
                            bytes[o] = 200u8.saturating_sub(t / 2); // B
                            bytes[o + 1] = 80; // G
                            bytes[o + 2] = 160u8.saturating_add(t / 3); // R
                            bytes[o + 3] = 255; // A
                        }
                    }
                }
            }

            let dup = match p.region.try_clone_fd() {
                Ok(fd) => fd,
                Err(e) => {
                    crate::warn!("surface {}: cannot dup shm fd: {e}", p.id);
                    return;
                }
            };
            let pool = shm.create_pool(dup.as_fd(), p.region.size() as i32, qh, ());
            // The buffer's user data is unused (we keep our own handle below).
            let buffer = pool.create_buffer(
                0,
                p.width as i32,
                p.height as i32,
                p.stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (),
            );
            pool.destroy();



            p.wl_surface.attach(Some(&buffer), 0, 0);
            p.wl_surface.damage(0, 0, p.width as i32, p.height as i32);
            p.xdg_surface.set_window_geometry(0, 0, p.width as i32, p.height as i32);
            p.wl_surface.commit();

            self.surfaces.insert(
                p.id,
                Surface {
                    wl_surface: p.wl_surface,
                    xdg_surface: Some(p.xdg_surface),
                    xdg_toplevel: self.toplevels.remove(&p.id),
                    subsurface: None,
                    buffer,
                    region: p.region,
                    dmabuf_fd: None,
                    width: p.width,
                    height: p.height,
                    configured: true,
                    closed: false,
                },
            );
            crate::info!("surface {} configured ({}×{})", p.id, p.width, p.height);
        }
    }

    /// The broker. Owns the Wayland connection and its event queue.
    pub struct Graphics {
        conn: Connection,
        queue: EventQueue<State>,
        state: State,
    }

    impl Graphics {
        /// Tear down a toplevel whose `configure` never arrived, so its proxies
        /// do not leak and its id is not reused while half-alive.
        fn abandon_pending(&mut self, id: u32) {
            if let Some(pos) = self.state.pending.iter().position(|p| p.id == id) {
                let p = self.state.pending.remove(pos);
                p.xdg_surface.destroy();
                p.wl_surface.destroy();
            }
            if let Some(t) = self.state.toplevels.remove(&id) {
                t.destroy();
            }
            let _ = self.conn.flush();
        }

        /// Connect to the compositor named by `WAYLAND_DISPLAY`. Returns `None`
        /// if there is no Wayland session (the daemon then runs graphics-less).
        pub fn connect() -> Option<Graphics> {
            let conn = match Connection::connect_to_env() {
                Ok(c) => c,
                Err(e) => {
                    crate::warn!("no Wayland session ({e}); graphics disabled");
                    return None;
                }
            };
            let (globals, mut queue) = match registry_queue_init::<State>(&conn) {
                Ok(v) => v,
                Err(e) => {
                    crate::warn!("Wayland registry init failed: {e}");
                    return None;
                }
            };
            let qh = queue.handle();
            let mut state = State::default();
            state.compositor = globals.bind(&qh, 1..=6, ()).ok();
            state.shm = globals.bind(&qh, 1..=1, ()).ok();
            state.wm_base = globals.bind(&qh, 1..=6, ()).ok();
            state.decoration = globals.bind(&qh, 1..=1, ()).ok();
            if state.decoration.is_some() {
                crate::info!("server-side decorations available (xdg-decoration)");
            } else {
                crate::info!("no xdg-decoration; the compositor draws its own title bar");
            }
            // v3 adds format modifiers, which we need to import GPU buffers.
            state.dmabuf = globals.bind(&qh, 3..=4, ()).ok();
            state.subcompositor = globals.bind(&qh, 1..=1, ()).ok();
            // Input: the seat gives us pointer and keyboard events, which are
            // forwarded to whichever guest owns the surface under the cursor.
            state.seat = globals.bind(&qh, 1..=7, ()).ok();
            if queue.roundtrip(&mut state).is_err() {
                crate::warn!("Wayland roundtrip failed");
                return None;
            }
            if state.compositor.is_none() || state.shm.is_none() || state.wm_base.is_none() {
                crate::warn!(
                    "Wayland missing globals (compositor={}, shm={}, xdg_wm_base={})",
                    state.compositor.is_some(),
                    state.shm.is_some(),
                    state.wm_base.is_some()
                );
                return None;
            }
            crate::info!("Wayland graphics broker ready");
            Some(Graphics { conn, queue, state })
        }

        /// Dispatch pending compositor events and return the input they carried.
        ///
        /// Called from the daemon's accept loop; the events are then pushed to
        /// the connection that subscribed for the surface.
        pub fn poll_input(&mut self) -> Vec<(u32, pengudroid_proto::InputKind)> {
            self.drain_wayland();
            std::mem::take(&mut self.state.input)
        }

        /// Read whatever the compositor has already sent, without blocking.
        ///
        /// `dispatch_pending` alone only replays events that a *previous* read
        /// put in the queue, so on its own it never sees anything new: the socket
        /// has to be read first. That was why pointer clicks did nothing — no
        /// `wl_pointer::Enter`/`Button` event ever reached the state machine, so
        /// `pointer_focus` stayed `None` and `push_input` dropped everything.
        ///
        /// The read is non-blocking: the daemon loop must stay responsive, and a
        /// `WouldBlock` simply means there was nothing to read.
        fn drain_wayland(&mut self) {
            // Never block: `poll(2)` first, so a blocking client socket cannot
            // stall the daemon loop.
            let fd = std::os::fd::AsRawFd::as_raw_fd(&self.conn.backend().poll_fd());
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ready > 0 && pfd.revents & libc::POLLIN != 0 {
                if let Some(guard) = self.queue.prepare_read() {
                    if let Err(e) = guard.read() {
                        let would_block = matches!(
                            &e,
                            wayland_client::backend::WaylandError::Io(io)
                                if io.kind() == std::io::ErrorKind::WouldBlock
                        );
                        if !would_block {
                            crate::warn!("wayland read: {e}");
                        }
                    }
                }
            }
            let _ = self.queue.dispatch_pending(&mut self.state);
            let _ = self.conn.flush();
        }

        /// The surface that currently has pointer focus, if any.
        ///
        /// Input injection needs it: the compositor is the only thing that
        /// normally decides which surface a pointer event belongs to.
        pub fn focused_surface(&self) -> Option<u32> {
            self.state.pointer_focus
        }

        /// The ids of every toplevel the compositor knows about.
        pub fn surface_ids(&self) -> Vec<u32> {
            self.state.surfaces.keys().copied().collect()
        }

        /// Create a toplevel-backed surface `width`×`height` with `title`.
        pub fn create_toplevel(
            &mut self,
            width: u32,
            height: u32,
            title: &str,
        ) -> Option<SurfaceInfo> {
            let (width, height) = (width.max(64), height.max(64));
            let stride = width * 4;
            let size = (stride * height) as u64;
            let name = format!("pengudroid-fb-{}", self.state.next_id + 1);
            let region = match AshmemRegion::create(&name, size) {
                Ok(r) => r,
                Err(e) => {
                    crate::warn!("cannot allocate surface memory: {e}");
                    return None;
                }
            };

            let compositor = self.state.compositor.clone()?;
            let wm_base = self.state.wm_base.clone()?;
            let qh = self.queue.handle();

            let id = self.state.alloc_id();
            let wl_surface = compositor.create_surface(&qh, ());
            let xdg_surface = wm_base.get_xdg_surface(&wl_surface, &qh, ());
            let xdg_toplevel = xdg_surface.get_toplevel(&qh, SurfaceId(id));
            xdg_toplevel.set_title(title.to_string());
            // Ask for a real title bar. The compositor may ignore it (GNOME
            // always draws client-side), which is why the request is best-effort.
            if let Some(manager) = self.state.decoration.clone() {
                let decoration = manager.get_toplevel_decoration(&xdg_toplevel, &qh, ());
                decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
                self.state.decorations.insert(id, decoration);
            }
            wl_surface.commit();

            self.state.pending.push(Pending {
                id,
                width,
                height,
                xdg_surface: xdg_surface.clone(),
                wl_surface,
                region,
                stride,
            });
            // Stash the toplevel proxy so `finalize_surface` can complete the
            // Surface without a second lookup path.
            self.state.toplevels.insert(id, xdg_toplevel);

            // Drive the configure → ack → attach → commit cycle synchronously.
            if let Err(e) = self.queue.roundtrip(&mut self.state) {
                crate::warn!("roundtrip during create failed: {e}");
                self.abandon_pending(id);
                return None;
            }

            let info = SurfaceInfo {
                id,
                shm_name: self
                    .state
                    .surfaces
                    .get(&id)
                    .map(|s| s.region.name().to_string())
                    .unwrap_or(name),
                stride,
                size,
                format: abi::ANDROID_RGBA_8888,
                width,
                height,
            };
            if !self.state.surfaces.contains_key(&id) {
                crate::warn!("surface {id} never got configured");
                self.abandon_pending(id);
                return None;
            }
            Some(info)
        }

        /// Re-commit a surface so pixels the guest wrote into the shared
        /// buffer become visible. Pixel contents are owned by the guest; the
        /// daemon only drives the Wayland commit.
        pub fn present(&mut self, id: u32) -> bool {
            // Drain pending events first: the compositor may have destroyed the
            // surface (or asked to close it) since the guest's last frame, and a
            // request to a dead object is a protocol error.
            self.poll();
            let Some(s) = self.state.surfaces.get(&id) else { return false };
            if s.closed || s.width == 0 || s.height == 0 {
                return false;
            }
            s.wl_surface.attach(Some(&s.buffer), 0, 0);
            // A zero-area damage rect is a protocol violation.
            s.wl_surface.damage(0, 0, s.width as i32, s.height as i32);
            s.wl_surface.commit();
            // Flush errors mean the compositor is gone; report the frame as lost
            // rather than pretending it was shown.
            self.conn.flush().is_ok()
        }

        /// Create a child layer of `parent` at `(x, y)`.
        ///
        /// Android composites a `SurfaceView` as its own layer under the
        /// window; the Wayland equivalent is a `wl_subsurface`. It is created in
        /// desynchronised mode so the guest can commit frames without the
        /// parent having to commit too (a video frame must not wait for the UI).
        pub fn create_subsurface(
            &mut self,
            parent: u32,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
        ) -> Option<u32> {
            let subcompositor = self.state.subcompositor.clone()?;
            let compositor = self.state.compositor.clone()?;
            let shm = self.state.shm.clone()?;
            let parent_surface = self.state.surfaces.get(&parent)?.wl_surface.clone();
            let qh = self.queue.handle();

            let id = self.state.alloc_id();
            let wl_surface = compositor.create_surface(&qh, ());
            let subsurface = subcompositor.get_subsurface(&wl_surface, &parent_surface, &qh, ());
            subsurface.set_position(x, y);
            subsurface.set_desync();

            // A subsurface still needs *a* buffer to be mapped; a 1x1
            // transparent one is attached until the first dma-buf arrives.
            let region = match AshmemRegion::create(&format!("pengudroid-sub-{id}"), 4) {
                Ok(r) => r,
                Err(e) => {
                    crate::warn!("cannot allocate subsurface memory: {e}");
                    subsurface.destroy();
                    wl_surface.destroy();
                    return None;
                }
            };
            let dup = region.try_clone_fd().ok()?;
            let pool = shm.create_pool(dup.as_fd(), region.size() as i32, &qh, ());
            let buffer = pool.create_buffer(
                0,
                1,
                1,
                4,
                wl_shm::Format::Argb8888,
                &qh,
                (),
            );
            pool.destroy();
            wl_surface.attach(Some(&buffer), 0, 0);
            wl_surface.damage(0, 0, 1, 1);
            wl_surface.commit();

            self.state.surfaces.insert(
                id,
                Surface {
                    wl_surface,
                    xdg_surface: None,
                    xdg_toplevel: None,
                    subsurface: Some(subsurface),
                    buffer,
                    region,
                    dmabuf_fd: None,
                    width: width.max(1),
                    height: height.max(1),
                    configured: true,
                    closed: false,
                },
            );
            let _ = self.conn.flush();
            crate::info!("created subsurface {id} of {parent} at {x},{y}");
            Some(id)
        }

        /// Move an existing subsurface inside its parent.
        pub fn set_subsurface_position(&mut self, id: u32, x: i32, y: i32) -> bool {
            let Some(s) = self.state.surfaces.get(&id) else { return false };
            let Some(sub) = s.subsurface.as_ref() else { return false };
            sub.set_position(x, y);
            s.wl_surface.commit();
            let _ = self.conn.flush();
            true
        }

        /// Destroy a surface and its Wayland objects (the compositor closed the
        /// window, or the guest went away). Idempotent.
        pub fn destroy_surface(&mut self, id: u32) -> bool {
            let Some(surface) = self.state.surfaces.remove(&id) else { return false };
            if let Some(t) = surface.xdg_toplevel {
                t.destroy();
            }
            if let Some(x) = surface.xdg_surface {
                x.destroy();
            }
            if let Some(sub) = surface.subsurface {
                sub.destroy();
            }
            surface.buffer.destroy();
            surface.wl_surface.destroy();
            let _ = self.conn.flush();
            crate::debug!("destroyed surface {id}");
            true
        }

        /// Process any queued events (non-blocking). Call periodically to learn
        /// about close requests.
        pub fn poll(&mut self) {
            self.drain_wayland();
        }

        pub fn close_requests(&mut self) -> Vec<u32> {
            self.poll();
            std::mem::take(&mut self.state.close_requests)
        }

        pub fn surface_close_requested(&self, id: u32) -> bool {
            self.state.surfaces.get(&id).map(|s| s.closed).unwrap_or(false)
        }

        /// Duplicate the backing memory fd for a surface so it can be sent to a
        /// guest (which mmaps it to render directly into the compositor's view).
        pub fn surface_fd(&self, id: u32) -> Option<std::os::fd::OwnedFd> {
            self.state.surfaces.get(&id).and_then(|s| s.region.try_clone_fd().ok())
        }

        /// Whether the compositor advertises `zwp_linux_dmabuf_v1` (v3+).
        pub fn dmabuf_available(&self) -> bool {
            self.state.dmabuf.is_some()
        }

        /// Number of dmabuf formats the compositor advertised.
        pub fn dmabuf_format_count(&self) -> u32 {
            self.state.dmabuf_supported.len() as u32
        }

        /// Whether the compositor advertised this exact `(fourcc, modifier)`.
        /// A modifier of 0 matches a plain `format` advertisement.
        pub fn dmabuf_supports(&self, fourcc: u32, modifier: u64) -> bool {
            self.state
                .dmabuf_supported
                .iter()
                .any(|(f, m)| *f == fourcc && (*m == modifier || *m == 0))
        }

        /// Process pending Wayland events (a blocking round-trip). Mostly useful
        /// to let the initial `format`/`modifier` burst from
        /// `zwp_linux_dmabuf_v1` be delivered before reading diagnostics.
        pub fn refresh(&mut self) {
            let _ = self.queue.roundtrip(&mut self.state);
        }

        /// Present a GPU buffer directly (no `wl_shm`, no CPU copy).
        ///
        /// Creates a throwaway toplevel, imports `fd` with
        /// `zwp_linux_dmabuf_v1` at the given geometry/format/modifier, attaches
        /// it, commits, and round-trips to learn whether the compositor accepted
        /// the buffer. This is the zero-copy counterpart of `create_toplevel`.
        #[allow(clippy::too_many_arguments)]
        pub fn dmabuf_selftest(
            &mut self,
            width: u32,
            height: u32,
            fd: std::os::fd::OwnedFd,
            stride: u32,
            fourcc: u32,
            modifier: u64,
            offset: u32,
        ) -> Result<(), String> {
            let compositor = self.state.compositor.clone().ok_or("no wl_compositor")?;
            let wm_base = self.state.wm_base.clone().ok_or("no xdg_wm_base")?;
            let dmabuf = self
                .state
                .dmabuf
                .clone()
                .ok_or("compositor lacks zwp_linux_dmabuf_v1 (v3+)")?;
            let qh = self.queue.handle();

            let wl_surface = compositor.create_surface(&qh, ());
            let xdg_surface = wm_base.get_xdg_surface(&wl_surface, &qh, ());
            // Throwaway toplevel for the dmabuf self-test: it owns no surface id.
            let toplevel = xdg_surface.get_toplevel(&qh, SurfaceId(u32::MAX));
            toplevel.set_title("PenguDroid dmabuf selftest".to_string());
            wl_surface.commit();

            self.state.dmabuf_created = false;
            self.state.dmabuf_failed = false;

            let params = dmabuf.create_params(&qh, ());
            params.add(
                fd.as_fd(),
                0,
                offset,
                stride,
                (modifier >> 32) as u32,
                (modifier & 0xffff_ffff) as u32,
            );
            let buffer = params.create_immed(
                width as i32,
                height as i32,
                fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &qh,
                (),
            );
            params.destroy();

            wl_surface.attach(Some(&buffer), 0, 0);
            wl_surface.damage(0, 0, width as i32, height as i32);
            wl_surface.commit();

            self.queue
                .roundtrip(&mut self.state)
                .map_err(|e| format!("roundtrip: {e}"))?;
            crate::debug!(
                "dmabuf selftest: created={} failed={}",
                self.state.dmabuf_created,
                self.state.dmabuf_failed
            );
            if self.state.dmabuf_failed {
                return Err("compositor rejected the dmabuf".to_string());
            }
            Ok(())
        }

        /// Present a GPU-rendered dma-buf on an existing surface (zero-copy).
        ///
        /// This is what a guest's `eglSwapBuffers` ultimately becomes: a buffer
        /// the GPU wrote, imported by the compositor without a CPU copy.
        #[allow(clippy::too_many_arguments)]
        pub fn present_dmabuf(
            &mut self,
            id: u32,
            fd: std::os::fd::OwnedFd,
            width: u32,
            height: u32,
            stride: u32,
            fourcc: u32,
            modifier: u64,
            offset: u32,
        ) -> Result<(), String> {
            let dmabuf = self
                .state
                .dmabuf
                .clone()
                .ok_or("compositor lacks zwp_linux_dmabuf_v1")?;
            let qh = self.queue.handle();
            let Some(s) = self.state.surfaces.get_mut(&id) else {
                return Err(format!("no such surface {id}"));
            };
            let params = dmabuf.create_params(&qh, ());
            params.add(
                fd.as_fd(),
                0,
                offset,
                stride,
                (modifier >> 32) as u32,
                (modifier & 0xffff_ffff) as u32,
            );
            let buffer = params.create_immed(
                width as i32,
                height as i32,
                fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &qh,
                (),
            );
            params.destroy();

            // The guest renders into a GL framebuffer, whose first row in memory is
            // the *bottom* of the image; `zwp_linux_dmabuf_v1` buffers are defined
            // top row first, like `wl_shm`. Without this the window comes out
            // upside down (the app bar along the bottom, text rotated 180°).
            //
            // `flipped_180` is "flip about the vertical axis, then rotate 180°",
            // which composes to a flip about the horizontal axis — a vertical flip,
            // exactly what is needed here. It is free: the compositor applies it
            // while sampling, so nothing is copied.
            //
            // `PENGUDROID_NO_PRESENT_FLIP=1` turns it off, for a compositor that
            // ignores buffer transforms or a guest that already renders top-down.
            //
            // A subsurface is fed by a decoder writing rows top-down, so it must
            // not be flipped.
            if s.subsurface.is_none() && std::env::var_os("PENGUDROID_NO_PRESENT_FLIP").is_none() {
                s.wl_surface
                    .set_buffer_transform(wl_output::Transform::Flipped180);
            }
            s.wl_surface.attach(Some(&buffer), 0, 0);
            s.wl_surface.damage(0, 0, width as i32, height as i32);
            s.wl_surface.commit();
            s.buffer = buffer;
            s.dmabuf_fd = Some(fd);
            let _ = self.conn.flush();

            // FPS meter: the daemon is the only component that sees every
            // presented frame, so it is the honest place to count them. The
            // window title is used as the display because it needs no overlay
            // rendering and every compositor shows it.
            if std::env::var_os("PENGUDROID_FPS_METER").is_some() {
                let now = std::time::Instant::now();
                let entry = self.state.fps.entry(id).or_insert((0, now));
                entry.0 += 1;
                let elapsed = now.duration_since(entry.1).as_secs_f32();
                if elapsed >= 1.0 {
                    let fps = entry.0 as f32 / elapsed;
                    entry.0 = 0;
                    entry.1 = now;
                    if let Some(toplevel) = self.state.toplevels.get(&id) {
                        toplevel.set_title(format!("PenguDroid \u{2014} {fps:.0} fps"));
                    }
                }
            }
            Ok(())
        }
    }

    impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
        fn event(
            state: &mut Self,
            _proxy: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
            event: zwp_linux_dmabuf_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                zwp_linux_dmabuf_v1::Event::Format { format } => {
                    state.dmabuf_formats = state.dmabuf_formats.saturating_add(1);
                    if !state.dmabuf_supported.iter().any(|(f, _)| *f == format) {
                        state.dmabuf_supported.push((format, 0));
                    }
                }
                zwp_linux_dmabuf_v1::Event::Modifier { format, modifier_hi, modifier_lo } => {
                    state.dmabuf_formats = state.dmabuf_formats.saturating_add(1);
                    let modifier = ((modifier_hi as u64) << 32) | modifier_lo as u64;
                    if !state.dmabuf_supported.contains(&(format, modifier)) {
                        state.dmabuf_supported.push((format, modifier));
                    }
                }
                _ => {}
            }
        }
    }

    impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()> for State {
        fn event(
            state: &mut Self,
            _proxy: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
            event: zwp_linux_buffer_params_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                zwp_linux_buffer_params_v1::Event::Created { .. } => state.dmabuf_created = true,
                zwp_linux_buffer_params_v1::Event::Failed => state.dmabuf_failed = true,
                _ => {}
            }
        }
    }

    // --- Dispatch implementations -------------------------------------------

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // Global discovery is handled by `registry_queue_init`.
        }
    }

    macro_rules! ignore_events {
        ($iface:ty, $udata:ty) => {
            impl Dispatch<$iface, $udata> for State {
                fn event(
                    _: &mut Self,
                    _: &$iface,
                    _: <$iface as wayland_client::Proxy>::Event,
                    _: &$udata,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        };
    }

    ignore_events!(wl_compositor::WlCompositor, ());
    ignore_events!(wl_shm::WlShm, ());
    ignore_events!(wl_shm_pool::WlShmPool, ());
    ignore_events!(wl_buffer::WlBuffer, ());
    ignore_events!(wl_surface::WlSurface, ());
    ignore_events!(wl_callback::WlCallback, ());
    ignore_events!(wl_subcompositor::WlSubcompositor, ());
    ignore_events!(wl_subsurface::WlSubsurface, ());

    impl Dispatch<wl_seat::WlSeat, ()> for State {
        fn event(
            state: &mut Self,
            seat: &wl_seat::WlSeat,
            event: wl_seat::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_seat::Event::Capabilities { capabilities } = event {
                use wayland_client::protocol::wl_seat::Capability;
                use wayland_client::WEnum;
                let has = |c: Capability| matches!(capabilities, WEnum::Value(v) if v.contains(c));
                if has(Capability::Pointer) && state.pointer.is_none() {
                    state.pointer = Some(seat.get_pointer(qh, ()));
                    crate::info!("seat: pointer available");
                }
                if has(Capability::Keyboard) && state.keyboard.is_none() {
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                    crate::info!("seat: keyboard available");
                }
            }
        }
    }

    impl Dispatch<wl_pointer::WlPointer, ()> for State {
        fn event(
            state: &mut Self,
            _: &wl_pointer::WlPointer,
            event: wl_pointer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            use pengudroid_proto::InputKind;
            match event {
                wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } => {
                    state.pointer_focus = state.surface_id_of(&surface);
                    state.pointer_pos = (surface_x, surface_y);
                    crate::info!(
                        "pointer entered surface {:?} at {surface_x:.0},{surface_y:.0}",
                        state.pointer_focus
                    );
                    state.push_input(InputKind::PointerEnter {
                        x: surface_x as f32,
                        y: surface_y as f32,
                    });
                }
                wl_pointer::Event::Leave { .. } => {
                    state.push_input(InputKind::PointerLeave);
                    state.pointer_focus = None;
                }
                wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                    state.pointer_pos = (surface_x, surface_y);
                    state.push_input(InputKind::PointerMotion {
                        x: surface_x as f32,
                        y: surface_y as f32,
                    });
                }
                wl_pointer::Event::Button { button, state: button_state, .. } => {
                    use wayland_client::protocol::wl_pointer::ButtonState;
                    use wayland_client::WEnum;
                    state.push_input(InputKind::PointerButton {
                        button,
                        pressed: matches!(button_state, WEnum::Value(ButtonState::Pressed)),
                    });
                }
                wl_pointer::Event::Axis { axis, value, .. } => {
                    use wayland_client::protocol::wl_pointer::Axis;
                    use wayland_client::WEnum;
                    state.push_input(InputKind::PointerAxis {
                        axis: if matches!(axis, WEnum::Value(Axis::VerticalScroll)) { 0 } else { 1 },
                        value: value as f32,
                    });
                }
                _ => {}
            }
        }
    }

    impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
        fn event(
            state: &mut Self,
            _: &wl_keyboard::WlKeyboard,
            event: wl_keyboard::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_keyboard::Event::Key { key, state: key_state, .. } = event {
                use wayland_client::protocol::wl_keyboard::KeyState;
                use wayland_client::WEnum;
                state.push_input(pengudroid_proto::InputKind::Key {
                    keycode: key,
                    pressed: matches!(key_state, WEnum::Value(KeyState::Pressed)),
                });
            }
        }
    }

    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
        fn event(
            _: &mut Self,
            proxy: &xdg_wm_base::XdgWmBase,
            event: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // Compositor liveness ping: must be answered or it drops us.
            if let xdg_wm_base::Event::Ping { serial } = event {
                proxy.pong(serial);
            }
        }
    }

    impl Dispatch<xdg_surface::XdgSurface, ()> for State {
        fn event(
            state: &mut Self,
            proxy: &xdg_surface::XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = event {
                proxy.ack_configure(serial);
                state.finalize_surface(proxy, qh);
            }
        }
    }

    impl Dispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
            _: zxdg_decoration_manager_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // The manager has no events.
        }
    }

    impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
            event: zxdg_toplevel_decoration_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // `Configure` reports which mode the compositor settled on. We asked
            // for server-side and cannot draw client-side decorations, so there
            // is nothing to react to; the event is consumed to keep the queue
            // clean.
            let _ = event;
        }
    }

    impl Dispatch<xdg_toplevel::XdgToplevel, SurfaceId> for State {
        fn event(
            state: &mut Self,
            proxy: &xdg_toplevel::XdgToplevel,
            event: xdg_toplevel::Event,
            _: &SurfaceId,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_toplevel::Event::Close = event {
                // The toplevel is created with the surface id as its user data,
                // so a close is attributed to *its* window (picking "the first
                // open surface" would be wrong with several windows).
                match proxy.data::<SurfaceId>().map(|d| d.0) {
                    Some(id) => {
                        if let Some(s) = state.surfaces.get_mut(&id) {
                            if !s.closed {
                                s.closed = true;
                                state.close_requests.push(id);
                            }
                        }
                    }
                    None => crate::warn!("close event from an untracked toplevel"),
                }
            }
        }
    }
}

#[cfg(feature = "wayland")]
pub use imp::Graphics;

#[cfg(not(feature = "wayland"))]
mod imp {
    use super::SurfaceInfo;
    /// Graphics-disabled build.
    pub struct Graphics;
    impl Graphics {
        pub fn connect() -> Option<Graphics> {
            crate::warn!("built without the `wayland` feature; graphics disabled");
            None
        }
        pub fn create_toplevel(&mut self, _w: u32, _h: u32, _t: &str) -> Option<SurfaceInfo> {
            None
        }
        pub fn create_subsurface(
            &mut self,
            _parent: u32,
            _x: i32,
            _y: i32,
            _w: u32,
            _h: u32,
        ) -> Option<u32> {
            None
        }
        pub fn set_subsurface_position(&mut self, _id: u32, _x: i32, _y: i32) -> bool {
            false
        }
        pub fn present(&mut self, _id: u32) -> bool {
            false
        }
        pub fn poll(&mut self) {}
        pub fn poll_input(&mut self) -> Vec<(u32, pengudroid_proto::InputKind)> {
            Vec::new()
        }
        pub fn close_requests(&mut self) -> Vec<u32> {
            Vec::new()
        }
        pub fn surface_close_requested(&self, _id: u32) -> bool {
            false
        }
        pub fn surface_fd(&self, _id: u32) -> Option<std::os::fd::OwnedFd> {
            None
        }
        pub fn dmabuf_available(&self) -> bool {
            false
        }
        pub fn dmabuf_format_count(&self) -> u32 {
            0
        }
        pub fn dmabuf_supports(&self, _fourcc: u32, _modifier: u64) -> bool {
            false
        }
        pub fn refresh(&mut self) {}
        #[allow(clippy::too_many_arguments)]
        pub fn dmabuf_selftest(
            &mut self,
            _width: u32,
            _height: u32,
            _fd: std::os::fd::OwnedFd,
            _stride: u32,
            _fourcc: u32,
            _modifier: u64,
            _offset: u32,
        ) -> Result<(), String> {
            Err("graphics built without the `wayland` feature".into())
        }
        #[allow(clippy::too_many_arguments)]
        pub fn present_dmabuf(
            &mut self,
            _id: u32,
            _fd: std::os::fd::OwnedFd,
            _width: u32,
            _height: u32,
            _stride: u32,
            _fourcc: u32,
            _modifier: u64,
            _offset: u32,
        ) -> Result<(), String> {
            Err("graphics built without the `wayland` feature".into())
        }
    }
}

#[cfg(not(feature = "wayland"))]
pub use imp::Graphics;
