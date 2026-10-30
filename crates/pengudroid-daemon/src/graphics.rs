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
    use std::num::NonZeroU32;
    use std::sync::Arc;
    use sctk_adwaita::{AdwaitaFrame, FrameConfig};
    use smithay_client_toolkit::{compositor::{CompositorState, SurfaceData},
        subcompositor::SubcompositorState, shm::Shm, shell::WaylandSurface};
    use smithay_client_toolkit::reexports::csd_frame::{DecorationsFrame, FrameAction, FrameClick, WindowState, CursorIcon, WindowManagerCapabilities};
    use wayland_protocols::wp::viewporter::client::{wp_viewporter, wp_viewport};
    use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
    use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_manager_v1, wp_cursor_shape_device_v1};
    use std::collections::HashMap;
    use std::os::fd::AsFd;

    use pengudroid_proto::{abi, BufferCrop};
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_region,
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
        pub parent_id: Option<u32>,
        pub buffer: wl_buffer::WlBuffer,
        /// Kept alive so the memfd is not closed while the compositor may read
        /// it, and so `present` can repaint.
        pub region: AshmemRegion,
        /// When the surface is fed by an imported dma-buf, its fd is kept alive
        /// here (the compositor holds its own reference, but the buffer's memory
        /// must stay valid while attached).
        pub dmabuf_fd: Option<std::os::fd::OwnedFd>,
        pending_frames: Vec<wl_buffer::WlBuffer>,
        pub width: u32,
        pub height: u32,
        pub configured: bool,
        pub closed: bool,
        pub fps_meter: bool,
        pub title: String,
        /// Guest pixels are independent of the compositor's content dimensions.
        pub display_width: u32,
        pub display_height: u32,
        pub viewport: Option<wp_viewport::WpViewport>,
        pub position: (i32, i32),
        pub window_state: WindowState,
        desktop_configured: bool,
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
        title: String,
    }

    #[derive(Default)]
    struct OutputSize {
        pixels: (u32, u32),
        scale: u32,
        rotated: bool,
        logical: Option<(u32, u32)>,
    }

    fn fit_window(source: (u32, u32), bounds: (u32, u32)) -> (u32, u32) {
        let (w, h) = (source.0.max(1), source.1.max(1));
        let (max_w, max_h) = (bounds.0.max(1), bounds.1.max(1));
        if w <= max_w && h <= max_h { return (w, h); }
        if (w as u64 * max_h as u64) > (h as u64 * max_w as u64) {
            (max_w, (h as u64 * max_w as u64 / w as u64).max(1) as u32)
        } else {
            ((w as u64 * max_h as u64 / h as u64).max(1) as u32, max_h)
        }
    }

    struct FrameSurface(wl_surface::WlSurface);
    impl WaylandSurface for FrameSurface {
        fn wl_surface(&self) -> &wl_surface::WlSurface { &self.0 }
    }

    #[derive(Default)]
    pub struct State {
        pub compositor: Option<wl_compositor::WlCompositor>,
        pub shm: Option<wl_shm::WlShm>,
        pub wm_base: Option<xdg_wm_base::XdgWmBase>,
        viewporter: Option<wp_viewporter::WpViewporter>,
        cursor_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
        cursor: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
        pointer_serial: u32,
        output_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
        outputs: HashMap<u32, OutputSize>,
        output_proxies: HashMap<u32, (wl_output::WlOutput, Option<zxdg_output_v1::ZxdgOutputV1>)>,
        frame_compositor: Option<Arc<CompositorState>>,
        frame_subcompositor: Option<Arc<SubcompositorState>>,
        frame_shm: Option<Shm>,
        frames: HashMap<u32, AdwaitaFrame<State>>,
        client_decorated: std::collections::HashSet<u32>,
        configured: HashMap<u32, (i32, i32, WindowState)>,
        wm_capabilities: HashMap<u32, WindowManagerCapabilities>,
        frame_focus: Option<(u32, wayland_client::backend::ObjectId)>,
        /// `zxdg_decoration_manager_v1`, when the compositor offers it.
        ///
        /// Compositors offering server-side decorations can supply a native
        /// title bar. GNOME/Mutter usually requires client-side decorations;
        /// that fallback must be supplied by this client, not the compositor.
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
        keyboard_focus: Option<u32>,
        held_keys: std::collections::HashSet<u32>,
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

        fn keyboard_key(&mut self, keycode: u32, pressed: bool) {
            if pressed { self.held_keys.insert(keycode); } else { self.held_keys.remove(&keycode); }
            if let Some(id) = self.keyboard_focus {
                self.input.push((id, pengudroid_proto::InputKind::Key { keycode, pressed }));
            }
        }

        fn release_keyboard(&mut self) {
            if let Some(id) = self.keyboard_focus {
                for keycode in self.held_keys.drain() {
                    self.input.push((id, pengudroid_proto::InputKind::Key { keycode, pressed: false }));
                }
            }
            self.held_keys.clear();
            self.keyboard_focus = None;
        }

        fn request_close(&mut self, id: u32) {
            if let Some(surface) = self.surfaces.get_mut(&id) {
                if !surface.closed {
                    surface.closed = true;
                    self.close_requests.push(id);
                }
            }
        }

        fn redraw_frame(&mut self, id: u32) {
            if let Some(frame) = self.frames.get_mut(&id) {
                if frame.is_dirty() { frame.draw(); }
            }
            if let Some(surface) = self.surfaces.get(&id) { surface.wl_surface.commit(); }
        }

        fn scale_children(&mut self, parent: u32) {
            let Some(root) = self.surfaces.get(&parent) else { return };
            let (source, display) = ((root.width, root.height), (root.display_width, root.display_height));
            for surface in self.surfaces.values_mut().filter(|s| s.parent_id == Some(parent)) {
                let scale = |value: u32, axis: usize| -> u32 {
                    let (from, to) = if axis == 0 { (source.0, display.0) } else { (source.1, display.1) };
                    ((value as u64 * to as u64) / from.max(1) as u64).max(1) as u32
                };
                surface.display_width = scale(surface.width, 0);
                surface.display_height = scale(surface.height, 1);
                if let Some(viewport) = &surface.viewport {
                    viewport.set_destination(surface.display_width as i32, surface.display_height as i32);
                }
                if let Some(sub) = &surface.subsurface {
                    sub.set_position(
                        (surface.position.0 as i64 * display.0 as i64 / source.0.max(1) as i64) as i32,
                        (surface.position.1 as i64 * display.1 as i64 / source.1.max(1) as i64) as i32,
                    );
                }
                surface.wl_surface.commit();
            }
        }

        fn configure_window(&mut self, id: u32, qh: &QueueHandle<Self>) {
            let (width, height, flags) = self.configured.remove(&id).unwrap_or((0, 0, WindowState::empty()));
            let Some(surface) = self.surfaces.get(&id) else { return };
            if self.client_decorated.contains(&id) && !self.frames.contains_key(&id) {
                if let (Some(shm), Some(compositor), Some(subcompositor)) =
                    (&self.frame_shm, &self.frame_compositor, &self.frame_subcompositor) {
                    match AdwaitaFrame::new(&FrameSurface(surface.wl_surface.clone()), shm,
                        Arc::clone(compositor), Arc::clone(subcompositor), qh.clone(), FrameConfig::auto()) {
                        Ok(mut frame) => {
                            frame.set_title(surface.title.clone());
                            frame.set_resizable(self.viewporter.is_some());
                            let mut capabilities = self.wm_capabilities.get(&id).copied().unwrap_or(WindowManagerCapabilities::all());
                            if self.viewporter.is_none() { capabilities.remove(WindowManagerCapabilities::MAXIMIZE | WindowManagerCapabilities::FULLSCREEN); }
                            frame.update_wm_capabilities(capabilities);
                            self.frames.insert(id, frame);
                            crate::info!("host client-side frame created for surface {id}");
                        }
                        Err(e) => crate::warn!("cannot create desktop frame for {id}: {e}"),
                    }
                }
            }
            let (mut content_w, mut content_h) = (surface.display_width, surface.display_height);
            // Start portrait Android windows inside the desktop instead of
            // putting their bottom controls below the monitor's visible area.
            if !surface.desktop_configured && surface.viewport.is_some() && width <= 0 && height <= 0 {
                let logical: Vec<_> = self.outputs.values().filter_map(|output| {
                    if let Some(size) = output.logical { return Some(size); }
                    let (w, h) = if output.rotated { (output.pixels.1, output.pixels.0) } else { output.pixels };
                    (w > 0 && h > 0).then_some((w / output.scale.max(1), h / output.scale.max(1)))
                }).collect();
                if let (Some(max_w), Some(max_h)) = (logical.iter().map(|(w, _)| *w).min(), logical.iter().map(|(_, h)| *h).min()) {
                    let header = self.frames.get(&id).map(|frame| frame.add_borders(0, 0).1).unwrap_or(0);
                    (content_w, content_h) = fit_window((content_w, content_h),
                        ((max_w * 85 / 100).max(1), (max_h * 85 / 100).saturating_sub(header).max(1)));
                }
            }
            if let Some(frame) = self.frames.get_mut(&id) {
                frame.update_state(flags);
                frame.set_hidden(flags.contains(WindowState::FULLSCREEN));
                if let (Some(w), Some(h)) = (NonZeroU32::new(width.max(0) as u32), NonZeroU32::new(height.max(0) as u32)) {
                    let (w, h) = frame.subtract_borders(w, h);
                    if let Some(w) = w { content_w = w.get(); }
                    if let Some(h) = h { content_h = h.get(); }
                } else {
                    if width > 0 { content_w = width as u32; }
                    if height > 0 {
                        let header = frame.add_borders(0, 0).1;
                        content_h = (height as u32).saturating_sub(header).max(1);
                    }
                }
                if surface.viewport.is_none() { content_w = surface.width; content_h = surface.height; }
                if !frame.is_hidden() {
                    frame.resize(NonZeroU32::new(content_w.max(1)).unwrap(), NonZeroU32::new(content_h.max(1)).unwrap());
                }
            } else {
                if width > 0 { content_w = width as u32; }
                if height > 0 { content_h = height as u32; }
            }
            let surface = self.surfaces.get_mut(&id).unwrap();
            surface.window_state = flags;
            surface.desktop_configured = true;
            if let Some(viewport) = &surface.viewport {
                surface.display_width = content_w.max(1);
                surface.display_height = content_h.max(1);
                viewport.set_destination(surface.display_width as i32, surface.display_height as i32);
            }
            let (x, y, w, h) = match self.frames.get(&id) {
                Some(frame) => {
                    let (x, y) = frame.location();
                    let (w, h) = frame.add_borders(surface.display_width, surface.display_height);
                    (x, y, w, h)
                }
                None => (0, 0, surface.display_width, surface.display_height),
            };
            if let Some(xdg) = &surface.xdg_surface { xdg.set_window_geometry(x, y, w as i32, h as i32); }
            if surface.viewport.is_none() {
                if let Some(top) = &surface.xdg_toplevel {
                    top.set_min_size(w as i32, h as i32); top.set_max_size(w as i32, h as i32);
                }
            }
            self.scale_children(id);
            self.redraw_frame(id);
        }

        fn guest_coordinates(&self, id: u32, x: f64, y: f64) -> (f32, f32) {
            let Some(surface) = self.surfaces.get(&id) else { return (x as f32, y as f32) };
            (x as f32 * surface.width as f32 / surface.display_width.max(1) as f32,
             y as f32 * surface.height as f32 / surface.display_height.max(1) as f32)
        }

        fn set_cursor(&self, icon: CursorIcon) {
            use wp_cursor_shape_device_v1::Shape as S;
            use CursorIcon as C;
            let shape = match icon {
                C::NResize => S::NResize, C::SResize => S::SResize,
                C::EResize => S::EResize, C::WResize => S::WResize,
                C::NeResize => S::NeResize, C::NwResize => S::NwResize,
                C::SeResize => S::SeResize, C::SwResize => S::SwResize,
                C::EwResize => S::EwResize, C::NsResize => S::NsResize,
                C::NeswResize => S::NeswResize, C::NwseResize => S::NwseResize,
                _ => S::Default,
            };
            if let Some(cursor) = &self.cursor { cursor.set_shape(self.pointer_serial, shape); }
        }

        fn update_frame_title(&mut self, id: u32, title: String) {
            if let Some(surface) = self.surfaces.get(&id) {
                if let Some(top) = &surface.xdg_toplevel { top.set_title(title.clone()); }
            }
            if let Some(frame) = self.frames.get_mut(&id) { frame.set_title(title); }
            self.redraw_frame(id);
        }

        fn frame_enter(&mut self, surface: &wl_surface::WlSurface, x: f64, y: f64) -> bool {
            let mut found = None;
            for (id, frame) in &mut self.frames {
                if let Some(cursor) = frame.click_point_moved(std::time::Duration::ZERO, &surface.id(), x, y) {
                    found = Some((*id, cursor)); break;
                }
            }
            if let Some((id, cursor)) = found {
                self.set_cursor(cursor);
                self.frame_focus = Some((id, surface.id()));
                self.redraw_frame(id);
                true
            } else { false }
        }

        fn frame_motion(&mut self, time: u32, x: f64, y: f64) -> bool {
            let Some((id, surface)) = self.frame_focus.clone() else { return false };
            let cursor = self.frames.get_mut(&id).and_then(|frame|
                frame.click_point_moved(std::time::Duration::from_millis(time as u64), &surface, x, y));
            if let Some(cursor) = cursor { self.set_cursor(cursor); }
            self.redraw_frame(id);
            true
        }

        fn frame_button(&mut self, button: u32, pressed: bool, time: u32, serial: u32) -> bool {
            let Some((id, _)) = &self.frame_focus else { return false };
            let id = *id;
            let click = match button { 0x110 => FrameClick::Normal, 0x111 => FrameClick::Alternate, _ => return true };
            let action = self.frames.get_mut(&id).and_then(|frame|
                frame.on_click(std::time::Duration::from_millis(time as u64), click, pressed));
            if let Some(action) = action {
                if matches!(action, FrameAction::Close) { self.request_close(id); }
                else if let (Some(surface), Some(seat)) = (self.surfaces.get(&id), &self.seat) {
                    if let Some(top) = &surface.xdg_toplevel {
                        match action {
                            FrameAction::Move => top._move(seat, serial),
                            FrameAction::Minimize => top.set_minimized(),
                            FrameAction::Maximize => top.set_maximized(),
                            FrameAction::UnMaximize => top.unset_maximized(),
                            FrameAction::ShowMenu(x, y) => top.show_window_menu(seat, serial, x, y),
                            FrameAction::Resize(edge) => {
                                use smithay_client_toolkit::reexports::csd_frame::ResizeEdge as E;
                                let edge = match edge {
                                    E::Top => xdg_toplevel::ResizeEdge::Top, E::Bottom => xdg_toplevel::ResizeEdge::Bottom,
                                    E::Left => xdg_toplevel::ResizeEdge::Left, E::Right => xdg_toplevel::ResizeEdge::Right,
                                    E::TopLeft => xdg_toplevel::ResizeEdge::TopLeft, E::TopRight => xdg_toplevel::ResizeEdge::TopRight,
                                    E::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft, E::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
                                    _ => xdg_toplevel::ResizeEdge::None,
                                };
                                top.resize(seat, serial, edge);
                            }
                            _ => {}
                        }
                    }
                }
            }
            self.redraw_frame(id);
            true
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
                    wl_surface: p.wl_surface.clone(),
                    xdg_surface: Some(p.xdg_surface),
                    xdg_toplevel: self.toplevels.remove(&p.id),
                    subsurface: None,
                    parent_id: None,
                    buffer,
                    region: p.region,
                    dmabuf_fd: None,
                    pending_frames: Vec::new(),
                    width: p.width,
                    height: p.height,
                    configured: true,
                    closed: false,
                    fps_meter: false,
                    title: p.title,
                    display_width: p.width,
                    display_height: p.height,
                    viewport: self.viewporter.as_ref().map(|v| v.get_viewport(&p.wl_surface, qh, ())),
                    position: (0, 0),
                    window_state: WindowState::empty(),
                    desktop_configured: false,
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
            self.state.frames.remove(&id);
            self.state.client_decorated.remove(&id);
            self.state.configured.remove(&id);
            self.state.wm_capabilities.remove(&id);
            if let Some(decoration) = self.state.decorations.remove(&id) { decoration.destroy(); }
            if let Some(t) = self.state.toplevels.remove(&id) { t.destroy(); }
            if let Some(pos) = self.state.pending.iter().position(|p| p.id == id) {
                let p = self.state.pending.remove(pos);
                p.xdg_surface.destroy();
                p.wl_surface.destroy();
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
                crate::info!("no xdg-decoration; client-side title bar support is required");
            }
            // v3 adds format modifiers, which we need to import GPU buffers.
            // Bind v3 for the format/modifier events. v4 requires the feedback
            // protocol instead and no longer guarantees these advertisements.
            state.dmabuf = globals.bind(&qh, 3..=3, ()).ok();
            state.subcompositor = globals.bind(&qh, 1..=1, ()).ok();
            state.viewporter = globals.bind(&qh, 1..=1, ()).ok();
            state.cursor_manager = globals.bind(&qh, 1..=1, ()).ok();
            state.frame_compositor = CompositorState::bind(&globals, &qh).ok().map(Arc::new);
            if let Some(compositor) = state.frame_compositor.as_ref() {
                state.frame_subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh).ok().map(Arc::new);
            }
            state.frame_shm = state.shm.clone().map(Shm::from);
            state.output_manager = globals.bind(&qh, 1..=3, ()).ok();
            globals.contents().with_list(|list| {
                for global in list.iter().filter(|global| global.interface == "wl_output") {
                    let output: wl_output::WlOutput = globals.registry().bind(global.name, global.version.min(4), &qh, global.name);
                    let logical = state.output_manager.as_ref().map(|manager| manager.get_xdg_output(&output, &qh, global.name));
                    state.outputs.insert(global.name, OutputSize::default());
                    state.output_proxies.insert(global.name, (output, logical));
                }
            });
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
            let wl_surface = match self.state.frame_compositor.as_ref() {
                Some(compositor) => compositor.create_surface(&qh),
                None => compositor.create_surface(&qh, ()),
            };
            let xdg_surface = wm_base.get_xdg_surface(&wl_surface, &qh, ());
            let xdg_toplevel = xdg_surface.get_toplevel(&qh, SurfaceId(id));
            xdg_toplevel.set_title(title.to_string());
            // Prefer server-side framing where it is offered, and use a host
            // client-side frame when the compositor declines or lacks it.
            if let Some(manager) = self.state.decoration.clone() {
                let decoration = manager.get_toplevel_decoration(&xdg_toplevel, &qh, SurfaceId(id));
                decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
                self.state.decorations.insert(id, decoration);
            } else {
                self.state.client_decorated.insert(id);
            }
            // A viewport can scale existing guest buffers during interactive
            // resize. Without it, do not advertise unsupported resizing.
            if self.state.viewporter.is_none() {
                xdg_toplevel.set_min_size(width as i32, height as i32);
                xdg_toplevel.set_max_size(width as i32, height as i32);
            } else { xdg_toplevel.set_min_size(160, 120); }
            wl_surface.commit();

            self.state.pending.push(Pending {
                id,
                width,
                height,
                xdg_surface: xdg_surface.clone(),
                wl_surface,
                region,
                stride,
                title: title.to_owned(),
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

            if let Some(surface) = self.state.surfaces.get_mut(&id) { surface.title = title.to_owned(); }
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
            s.wl_surface.damage(0, 0, s.display_width as i32, s.display_height as i32);
            s.wl_surface.commit();
            // Flush errors mean the compositor is gone; report the frame as lost
            // rather than pretending it was shown.
            let presented = self.conn.flush().is_ok();
            if presented { self.record_frame(id); }
            presented
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
            // Android dispatches SurfaceView touch events through the parent
            // ViewRootImpl. This video-only child has no input subscription;
            // without an empty input region it swallows clicks over the video.
            let input_region = compositor.create_region(&qh, ());
            wl_surface.set_input_region(Some(&input_region));
            input_region.destroy();
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
                    wl_surface: wl_surface.clone(),
                    xdg_surface: None,
                    xdg_toplevel: None,
                    subsurface: Some(subsurface),
                    parent_id: Some(parent),
                    buffer,
                    region,
                    dmabuf_fd: None,
                    pending_frames: Vec::new(),
                    width: width.max(1),
                    height: height.max(1),
                    configured: true,
                    closed: false,
                    fps_meter: false,
                    title: String::new(),
                    display_width: width.max(1),
                    display_height: height.max(1),
                    viewport: self.state.viewporter.as_ref().map(|v| v.get_viewport(&wl_surface, &qh, ())),
                    position: (x, y),
                    window_state: WindowState::empty(),
                    desktop_configured: true,
                },
            );
            self.state.scale_children(parent);
            self.state.surfaces.get(&parent)?.wl_surface.commit();
            let _ = self.conn.flush();
            crate::info!("created subsurface {id} of {parent} at {x},{y} size={width}x{height} (input transparent)");
            Some(id)
        }

        /// Move an existing subsurface inside its parent.
        pub fn set_subsurface_position(&mut self, id: u32, x: i32, y: i32) -> bool {
            let Some(s) = self.state.surfaces.get_mut(&id) else { return false };
            if s.subsurface.is_none() { return false; }
            s.position = (x, y);
            let Some(parent) = s.parent_id else { return false };
            self.state.scale_children(parent);
            self.state.surfaces.get(&parent).unwrap().wl_surface.commit();
            let _ = self.conn.flush();
            true
        }

        pub fn set_surface_options(&mut self, id: u32, enabled: bool) -> bool {
            let Some(surface) = self.state.surfaces.get_mut(&id) else { return false };
            surface.fps_meter = enabled;
            let title = surface.title.clone();
            if !enabled { self.state.update_frame_title(id, title); }
            let _ = self.conn.flush();
            true
        }

        fn record_frame(&mut self, id: u32) {
            let Some(surface) = self.state.surfaces.get(&id) else { return };
            let parent = surface.parent_id.unwrap_or(id);
            let Some(root) = self.state.surfaces.get(&parent) else { return };
            if !root.fps_meter { return; }
            // If video has a separate child layer, count decoder frames rather
            // than summing UI swaps with video frames.
            if id == parent && self.state.surfaces.values().any(|s| s.parent_id == Some(parent)) { return; }
            let now = std::time::Instant::now();
            let entry = self.state.fps.entry(id).or_insert((0, now));
            entry.0 += 1;
            let elapsed = now.duration_since(entry.1).as_secs_f32();
            if elapsed >= 1.0 {
                let fps = entry.0 as f32 / elapsed;
                entry.0 = 0; entry.1 = now;
                let title = format!("{} · {fps:.0} FPS", root.title);
                self.state.update_frame_title(parent, title);
                let _ = self.conn.flush();
            }
        }

        pub fn contains_surface(&self, id: u32) -> bool {
            self.state.surfaces.contains_key(&id)
        }

        pub fn is_toplevel(&self, id: u32) -> bool {
            self.state.surfaces.get(&id)
                .is_some_and(|s| s.xdg_toplevel.is_some() && !s.closed)
        }

        /// Destroy a surface and its Wayland objects (the compositor closed the
        /// window, or the guest went away). Idempotent.
        pub fn destroy_surface(&mut self, id: u32) -> bool {
            // Retire children before their parent. Otherwise their commits are
            // still Acked even though Wayland can no longer display them.
            let children: Vec<u32> = self.state.surfaces.iter()
                .filter_map(|(child, s)| (s.parent_id == Some(id)).then_some(*child))
                .collect();
            for child in children {
                self.destroy_surface(child);
            }
            if self.state.keyboard_focus == Some(id) { self.state.release_keyboard(); }
            if self.state.pointer_focus == Some(id) { self.state.pointer_focus = None; }
            self.state.fps.remove(&id);
            self.state.frames.remove(&id);
            self.state.client_decorated.remove(&id);
            self.state.configured.remove(&id);
            self.state.wm_capabilities.remove(&id);
            if self.state.frame_focus.as_ref().is_some_and(|(owner, _)| *owner == id) { self.state.frame_focus = None; }
            if let Some(decoration) = self.state.decorations.remove(&id) { decoration.destroy(); }
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
            for buffer in surface.pending_frames {
                // A leased decode surface cannot be recycled until the real
                // compositor release, even after its wl_surface is destroyed.
                let leased = buffer.data::<FrameBuffer>().is_some_and(|data| data.released.is_some());
                if buffer.is_alive() && !leased { buffer.destroy(); }
            }
            surface.buffer.destroy();
            if let Some(viewport) = surface.viewport { viewport.destroy(); }
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

        // Keep compositor backpressure bounded, including when a window is hidden.
        fn frame_slot_available(&mut self, id: u32) -> Result<bool, String> {
            self.drain_wayland();
            let surface = self.state.surfaces.get_mut(&id).ok_or_else(|| format!("unknown surface {id}"))?;
            surface.pending_frames.retain(|buffer| buffer.is_alive());
            Ok(surface.pending_frames.len() < 3)
        }

        /// Snapshot decoder-owned CPU memory and submit it through wl_shm.
        /// Every frame has separate storage until the compositor releases it.
        pub fn present_shm(&mut self, id: u32, fd: std::os::fd::OwnedFd,
            width: u32, height: u32, stride: u32, format: u32, crop: BufferCrop) -> Result<(), String> {
            let source_size = crop.dimensions(width, height)?;
            if !self.frame_slot_available(id)? { return Ok(()); }
            let pixels = crate::software::snapshot(fd, width, height, stride, format, crop)?;
            let surface = self.state.surfaces.get(&id).unwrap();
            // wp_viewporter scales on the compositor/GPU. Only the legacy
            // path without a viewport needs a CPU-resized snapshot.
            let (width, height) = if surface.subsurface.is_some() && surface.viewport.is_none() {
                (surface.width, surface.height)
            } else { source_size };
            let pixels = crate::software::resize(pixels, source_size.0, source_size.1, width, height)?;
            if let Ok(dir) = std::env::var("PENGUDROID_DUMP_PRESENT") {
                // Diagnostic: proves what the compositor is about to attach,
                // including a YUV video frame after the CPU conversion.
                let path = std::path::Path::new(&dir)
                    .join(format!("present-{id}-{width}x{height}.ppm"));
                if std::fs::metadata(&path).is_err() {
                    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
                    for pixel in pixels.chunks_exact(4) {
                        out.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                    }
                    let _ = std::fs::write(&path, out);
                }
            }
            let region = AshmemRegion::create("pengudroid-video-snapshot", pixels.len() as u64)
                .map_err(|e| e.to_string())?;
            region.map_mut().map_err(|e| e.to_string())?.as_mut_slice().copy_from_slice(&pixels);
            let shm = self.state.shm.as_ref().ok_or("no wl_shm")?;
            let qh = self.queue.handle();
            let duplicate = region.try_clone_fd().map_err(|e| e.to_string())?;
            let pool = shm.create_pool(duplicate.as_fd(), pixels.len() as i32, &qh, ());
            let buffer = pool.create_buffer(0, width as i32, height as i32,
                (width * 4) as i32, wl_shm::Format::Xrgb8888, &qh, FrameBuffer::default());
            pool.destroy();
            let s = self.state.surfaces.get_mut(&id).unwrap();
            s.wl_surface.set_buffer_transform(wl_output::Transform::Normal);
            s.wl_surface.attach(Some(&buffer), 0, 0);
            s.wl_surface.damage(0, 0, s.display_width as i32, s.display_height as i32);
            s.wl_surface.commit();
            s.pending_frames.push(buffer);
            // The pool request sends its own fd. Wayland retains the snapshot
            // independently of the Android buffer, which may now be recycled.
            self.conn.flush().map_err(|e| e.to_string())?;
            self.record_frame(id);
            Ok(())
        }

        /// Import a synchronized, top-down multi-plane video frame without CPU
        /// conversion. Keep the source HardwareFrame until the returned flag is
        /// true (wl_buffer.release). None means dropped before import.
        pub fn present_video_dmabuf(&mut self, id: u32, frame: pengudroid_hostgpu::video::ExportedFrame)
            -> Result<Option<Arc<std::sync::atomic::AtomicBool>>, String> {
            frame.validate()?;
            if !self.frame_slot_available(id)? { return Ok(None); }
            for plane in &frame.planes {
                if !self.state.dmabuf_supported.contains(&(frame.fourcc, plane.modifier)) {
                    return Err(format!("compositor did not advertise video format {:#x} modifier {:#x}", frame.fourcc, plane.modifier));
                }
            }
            let dmabuf = self.state.dmabuf.as_ref().ok_or("compositor lacks linux-dmabuf")?;
            let qh = self.queue.handle();
            let params = dmabuf.create_params(&qh, ());
            for (index, plane) in frame.planes.iter().enumerate() {
                params.add(frame.objects[plane.object_index as usize].as_fd(), index as u32,
                    plane.offset, plane.stride, (plane.modifier >> 32) as u32, plane.modifier as u32);
            }
            let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let buffer = params.create_immed(frame.width as i32, frame.height as i32, frame.fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(), &qh, FrameBuffer { released: Some(released.clone()) });
            params.destroy();
            let surface = self.state.surfaces.get_mut(&id).ok_or("video surface disappeared")?;
            surface.wl_surface.set_buffer_transform(wl_output::Transform::Normal);
            surface.wl_surface.attach(Some(&buffer), 0, 0);
            surface.wl_surface.damage(0, 0, surface.display_width as i32, surface.display_height as i32);
            surface.wl_surface.commit();
            surface.pending_frames.push(buffer);
            self.conn.flush().map_err(|e| e.to_string())?;
            self.record_frame(id);
            Ok(Some(released))
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
            if !self.frame_slot_available(id)? { return Ok(()); }
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
                FrameBuffer::default(),
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
            s.wl_surface.damage(0, 0, s.display_width as i32, s.display_height as i32);
            s.wl_surface.commit();
            s.pending_frames.push(buffer);
            s.dmabuf_fd = Some(fd);
            let _ = self.conn.flush();

            self.record_frame(id);
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
        fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event,
            _: &GlobalListContents, _: &Connection, qh: &QueueHandle<Self>) {
            match event {
                wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
                    if !state.output_proxies.contains_key(&name) {
                        let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, name);
                        let logical = state.output_manager.as_ref().map(|manager| manager.get_xdg_output(&output, qh, name));
                        state.outputs.insert(name, OutputSize::default());
                        state.output_proxies.insert(name, (output, logical));
                    }
                }
                wl_registry::Event::GlobalRemove { name } => {
                    state.outputs.remove(&name);
                    if let Some((output, logical)) = state.output_proxies.remove(&name) {
                        if let Some(logical) = logical { logical.destroy(); }
                        if output.version() >= 3 { output.release(); }
                    }
                }
                _ => {}
            }
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

    /// Per-frame imports are retired only after wl_buffer.release.
    #[derive(Default)]
    struct FrameBuffer { released: Option<Arc<std::sync::atomic::AtomicBool>> }
    impl Dispatch<wl_buffer::WlBuffer, FrameBuffer> for State {
        fn event(_: &mut Self, buffer: &wl_buffer::WlBuffer, event: wl_buffer::Event,
            data: &FrameBuffer, _: &Connection, _: &QueueHandle<Self>) {
            if let wl_buffer::Event::Release = event {
                if let Some(released) = &data.released { released.store(true, std::sync::atomic::Ordering::Release); }
                buffer.destroy();
            }
        }
    }

    ignore_events!(zxdg_output_manager_v1::ZxdgOutputManagerV1, ());
    impl Dispatch<wl_output::WlOutput, u32> for State {
        fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event,
            id: &u32, _: &Connection, _: &QueueHandle<Self>) {
            let output = state.outputs.entry(*id).or_default();
            match event {
                wl_output::Event::Mode { flags, width, height, .. } => {
                    if matches!(flags, wayland_client::WEnum::Value(flags) if flags.contains(wl_output::Mode::Current)) {
                        output.pixels = (width.max(0) as u32, height.max(0) as u32);
                    }
                }
                wl_output::Event::Scale { factor } => output.scale = factor.max(1) as u32,
                wl_output::Event::Geometry { transform, .. } => {
                    output.rotated = matches!(transform, wayland_client::WEnum::Value(
                        wl_output::Transform::_90 | wl_output::Transform::_270 |
                        wl_output::Transform::Flipped90 | wl_output::Transform::Flipped270));
                }
                _ => {}
            }
        }
    }
    impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
        fn event(state: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, event: zxdg_output_v1::Event,
            id: &u32, _: &Connection, _: &QueueHandle<Self>) {
            if let zxdg_output_v1::Event::LogicalSize { width, height } = event {
                if width > 0 && height > 0 { state.outputs.entry(*id).or_default().logical = Some((width as u32, height as u32)); }
            }
        }
    }

    ignore_events!(wp_viewporter::WpViewporter, ());
    ignore_events!(wp_cursor_shape_manager_v1::WpCursorShapeManagerV1, ());
    ignore_events!(wp_cursor_shape_device_v1::WpCursorShapeDeviceV1, ());
    ignore_events!(wp_viewport::WpViewport, ());
    ignore_events!(wl_compositor::WlCompositor, smithay_client_toolkit::globals::GlobalData);
    smithay_client_toolkit::delegate_subcompositor!(State);
    impl Dispatch<wl_surface::WlSurface, SurfaceData> for State {
        fn event(state: &mut Self, surface: &wl_surface::WlSurface, event: wl_surface::Event,
            _: &SurfaceData, _: &Connection, _: &QueueHandle<Self>) {
            if let wl_surface::Event::PreferredBufferScale { factor } = event {
                if let Some(id) = state.surface_id_of(surface) {
                    if let Some(frame) = state.frames.get_mut(&id) { frame.set_scaling_factor(factor as f64); }
                    state.redraw_frame(id);
                }
            }
        }
    }

    ignore_events!(wl_compositor::WlCompositor, ());
    ignore_events!(wl_shm::WlShm, ());
    ignore_events!(wl_shm_pool::WlShmPool, ());
    ignore_events!(wl_buffer::WlBuffer, ());
    ignore_events!(wl_surface::WlSurface, ());
    ignore_events!(wl_region::WlRegion, ());
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
                    let pointer = seat.get_pointer(qh, ());
                    state.cursor = state.cursor_manager.as_ref().map(|manager| manager.get_pointer(&pointer, qh, ()));
                    state.pointer = Some(pointer);
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
                wl_pointer::Event::Enter { surface, surface_x, surface_y, serial } => {
                    state.pointer_serial = serial;
                    state.set_cursor(CursorIcon::Default);
                    state.frame_focus = None;
                    if state.frame_enter(&surface, surface_x, surface_y) {
                        state.pointer_focus = None;
                        return;
                    }
                    state.pointer_focus = state.surface_id_of(&surface);
                    state.pointer_pos = (surface_x, surface_y);
                    crate::info!(
                        "pointer entered surface {:?} at {surface_x:.0},{surface_y:.0}",
                        state.pointer_focus
                    );
                    let (x, y) = state.pointer_focus.map(|id| state.guest_coordinates(id, surface_x, surface_y))
                        .unwrap_or((surface_x as f32, surface_y as f32));
                    state.push_input(InputKind::PointerEnter { x, y });
                }
                wl_pointer::Event::Leave { .. } => {
                    if let Some((id, _)) = state.frame_focus.take() {
                        if let Some(frame) = state.frames.get_mut(&id) { frame.click_point_left(); }
                        state.redraw_frame(id);
                    }
                    state.push_input(InputKind::PointerLeave);
                    state.pointer_focus = None;
                }
                wl_pointer::Event::Motion { surface_x, surface_y, time } => {
                    if state.frame_motion(time, surface_x, surface_y) { return; }
                    state.pointer_pos = (surface_x, surface_y);
                    let (x, y) = state.pointer_focus.map(|id| state.guest_coordinates(id, surface_x, surface_y))
                        .unwrap_or((surface_x as f32, surface_y as f32));
                    state.push_input(InputKind::PointerMotion { x, y });
                }
                wl_pointer::Event::Button { button, state: button_state, time, serial } => {
                    use wayland_client::protocol::wl_pointer::ButtonState;
                    use wayland_client::WEnum;
                    let pressed = matches!(button_state, WEnum::Value(ButtonState::Pressed));
                    if state.frame_button(button, pressed, time, serial) { return; }
                    state.push_input(InputKind::PointerButton { button, pressed });
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
            use pengudroid_proto::InputKind;
            use wayland_client::protocol::wl_keyboard::KeyState;
            use wayland_client::WEnum;
            match event {
                wl_keyboard::Event::Enter { surface, keys, .. } => {
                    state.release_keyboard();
                    state.keyboard_focus = state.surface_id_of(&surface);
                    // Compositor-provided held modifiers are physical keycodes,
                    // not characters. Restore them on this keyboard focus only.
                    if let Some(id) = state.keyboard_focus {
                        for key in keys.chunks_exact(4) {
                            let keycode = u32::from_ne_bytes(key.try_into().unwrap());
                            state.held_keys.insert(keycode);
                            state.input.push((id, InputKind::Key { keycode, pressed: true }));
                        }
                    }
                }
                wl_keyboard::Event::Leave { .. } => state.release_keyboard(),
                wl_keyboard::Event::Key { key, state: key_state, .. } => {
                    let pressed = matches!(key_state, WEnum::Value(KeyState::Pressed));
                    state.keyboard_key(key, pressed);
                }
                _ => {}
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
                if let Some(id) = state.surfaces.iter().find_map(|(id, s)|
                    (s.xdg_surface.as_ref() == Some(proxy)).then_some(*id)) {
                    state.configure_window(id, qh);
                }
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

    impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, SurfaceId> for State {
        fn event(state: &mut Self, _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
            event: zxdg_toplevel_decoration_v1::Event, id: &SurfaceId, _: &Connection, _: &QueueHandle<Self>) {
            if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
                if matches!(mode, wayland_client::WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide)) {
                    state.client_decorated.remove(&id.0);
                    state.frames.remove(&id.0);
                } else { state.client_decorated.insert(id.0); }
            }
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
            let Some(id) = proxy.data::<SurfaceId>().map(|data| data.0) else { return };
            match event {
                xdg_toplevel::Event::Close => state.request_close(id),
                xdg_toplevel::Event::Configure { width, height, states } => {
                    let mut flags = WindowState::empty();
                    for value in states.chunks_exact(4) {
                        flags |= match u32::from_ne_bytes(value.try_into().unwrap()) {
                            1 => WindowState::MAXIMIZED, 2 => WindowState::FULLSCREEN,
                            3 => WindowState::RESIZING, 4 => WindowState::ACTIVATED,
                            5 => WindowState::TILED_LEFT, 6 => WindowState::TILED_RIGHT,
                            7 => WindowState::TILED_TOP, 8 => WindowState::TILED_BOTTOM,
                            _ => WindowState::empty(),
                        };
                    }
                    state.configured.insert(id, (width, height, flags));
                }
                xdg_toplevel::Event::WmCapabilities { capabilities } => {
                    let mut flags = WindowManagerCapabilities::empty();
                    for value in capabilities.chunks_exact(4) {
                        flags |= match u32::from_ne_bytes(value.try_into().unwrap()) {
                            1 => WindowManagerCapabilities::WINDOW_MENU,
                            2 => WindowManagerCapabilities::MAXIMIZE,
                            3 => WindowManagerCapabilities::FULLSCREEN,
                            4 => WindowManagerCapabilities::MINIMIZE,
                            _ => WindowManagerCapabilities::empty(),
                        };
                    }
                    if state.viewporter.is_none() { flags.remove(WindowManagerCapabilities::MAXIMIZE | WindowManagerCapabilities::FULLSCREEN); }
                    state.wm_capabilities.insert(id, flags);
                    if let Some(frame) = state.frames.get_mut(&id) { frame.update_wm_capabilities(flags); }
                    state.redraw_frame(id);
                }
                _ => {}
            }
        }
    }
    #[cfg(test)]
    mod focus_tests {
        use super::*;
        use pengudroid_proto::InputKind;
        #[test]
        fn initial_portrait_window_fits_small_and_rotated_desktops() {
            assert_eq!(fit_window((720, 1280), (1000, 700)), (393, 700));
            assert_eq!(fit_window((1280, 720), (700, 1000)), (700, 393));
            assert_eq!(fit_window((320, 240), (1000, 700)), (320, 240));
        }

        #[test]
        fn hardware_video_import_preserves_planes_and_waits_for_release() {
            use pengudroid_hostgpu::video::{Decoder, ReceiveStatus, SendStatus};
            use std::sync::atomic::Ordering;
            let (Some(plugin), Some(fixture)) = (std::env::var_os("PENGUDROID_TEST_VIDEO_BRIDGE"),
                std::env::var_os("PENGUDROID_TEST_VIDEO_FIXTURE")) else { return; };
            let bytes = std::fs::read(fixture).unwrap();
            let device = std::env::var_os("PENGUDROID_TEST_VIDEO_DEVICE").unwrap_or_else(|| "/dev/dri/renderD128".into());
            let mut decoder = Decoder::open_plugin(std::path::Path::new(&plugin), "h264", std::path::Path::new(&device), &[]).unwrap();
            let mut graphics = Graphics::connect().expect("real compositor required for GPU video import test");
            graphics.refresh();
            let id = graphics.create_toplevel(128, 128, "PenguDroid hardware video test").unwrap().id;
            for pts in 0..32 {
                decoder.flush();
                assert_eq!(decoder.send(&bytes, pts).unwrap(), SendStatus::Accepted);
                assert_eq!(decoder.end_of_stream().unwrap(), SendStatus::Accepted);
                let frame = match decoder.receive().unwrap() {
                    ReceiveStatus::Frame(frame) => frame, _ => panic!("no hardware frame"),
                };
                let export = frame.export().unwrap();
                assert_eq!(export.planes.len(), 2, "NV12 planes must reach the compositor unchanged");
                let released = graphics.present_video_dmabuf(id, export).unwrap().expect("frame slot available");
                graphics.refresh();
                if pts == 31 { assert!(graphics.destroy_surface(id)); }
                else { assert!(graphics.present(id)); } // detach the video buffer
                for _ in 0..100 {
                    graphics.refresh();
                    if released.load(Ordering::Acquire) { break; }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                assert!(released.load(Ordering::Acquire), "source surface cannot be recycled before wl_buffer.release");
                drop(frame);
            }
            graphics.destroy_surface(id);
            graphics.refresh();
        }

        #[test]
        fn software_frames_remain_bounded_and_release_wayland_objects() {
            if std::env::var_os("WAYLAND_DISPLAY").is_none() { return; }
            let mut graphics = Graphics::connect().expect("desktop compositor");
            let id = graphics.create_toplevel(64, 64, "PenguDroid frame lifetime test").unwrap().id;
            let source = AshmemRegion::create("pd-frame-lifetime", 128 * 72 * 4).unwrap();
            let original = graphics.state.surfaces[&id].buffer.id();
            for _ in 0..1000 {
                graphics.present_shm(id, source.try_clone_fd().unwrap(), 128, 72, 512, 1, BufferCrop::full(128, 72)).unwrap();
                assert!(graphics.state.surfaces[&id].pending_frames.len() <= 3);
            }
            graphics.queue.roundtrip(&mut graphics.state).unwrap();
            let surface = &graphics.state.surfaces[&id];
            assert_eq!(surface.buffer.id(), original, "fallback buffer remains owned separately");
            // At most the currently attached buffer may still be retained once
            // the compositor has processed and released superseded commits.
            assert!(surface.pending_frames.iter().filter(|b| b.is_alive()).count() <= 1);
            graphics.destroy_surface(id);
            graphics.queue.roundtrip(&mut graphics.state).unwrap();
        }

        #[test]
        fn native_frame_resize_maximize_and_close_keep_video_and_input_aligned() {
            if std::env::var_os("WAYLAND_DISPLAY").is_none() {
                eprintln!("skipping: no Wayland compositor"); return;
            }
            let mut graphics = Graphics::connect().expect("connect to desktop compositor");
            if graphics.state.viewporter.is_none() {
                eprintln!("skipping: compositor does not provide wp_viewporter"); return;
            }
            let info = graphics.create_toplevel(320, 240, "PenguDroid desktop lifecycle test").unwrap();
            let id = info.id;
            if let Some(decoration) = graphics.state.decorations.get(&id) {
                decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide);
                graphics.queue.roundtrip(&mut graphics.state).unwrap();
            }
            graphics.state.client_decorated.insert(id);
            graphics.state.configured.insert(id, (640, 480, WindowState::ACTIVATED));
            graphics.state.configure_window(id, &graphics.queue.handle());
            graphics.queue.roundtrip(&mut graphics.state).unwrap();
            assert!(graphics.state.frames.contains_key(&id), "desktop frame must exist");
            let header = graphics.state.frames[&id].add_borders(0, 0).1;
            let root = &graphics.state.surfaces[&id];
            assert_eq!((root.width, root.height), (320, 240), "guest buffer remains valid");
            assert_eq!((root.display_width, root.display_height), (640, 480 - header));
            let (x, y) = graphics.state.guest_coordinates(id, 320., (480 - header) as f64 / 2.);
            assert!((x - 160.).abs() < 0.001 && (y - 120.).abs() < 0.001);
            let video = graphics.create_subsurface(id, 16, 20, 160, 90).unwrap();
            let child = &graphics.state.surfaces[&video];
            assert_eq!(child.display_width, 320);
            assert_eq!(child.display_height, (90 * (480 - header) / 240).max(1));
            let top = graphics.state.surfaces[&id].xdg_toplevel.clone().unwrap();
            for maximized in [true, false] {
                if maximized { top.set_maximized(); } else { top.unset_maximized(); }
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    graphics.queue.roundtrip(&mut graphics.state).unwrap();
                    if graphics.state.surfaces[&id].window_state.contains(WindowState::MAXIMIZED) == maximized { break; }
                    assert!(std::time::Instant::now() < deadline, "compositor did not configure maximize={maximized}");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            graphics.state.request_close(id);
            graphics.state.request_close(id);
            assert_eq!(graphics.close_requests(), vec![id]);
            assert!(!graphics.present(id), "closed windows reject new guest frames");
            graphics.destroy_surface(id);
            assert!(!graphics.state.frames.contains_key(&id));
            assert!(!graphics.contains_surface(video));
            graphics.queue.roundtrip(&mut graphics.state).unwrap();
        }

        #[test]
        fn keyboard_focus_is_independent_of_mouse_and_leave_releases_modifiers() {
            let mut state = State::default();
            state.pointer_focus = Some(10);
            state.keyboard_focus = Some(20);
            state.keyboard_key(42, true);
            state.keyboard_key(30, true);
            assert!(state.input.iter().all(|(id, _)| *id == 20));
            state.input.clear();
            state.release_keyboard();
            assert_eq!(state.input.len(), 2);
            assert!(state.input.iter().all(|(id, event)| *id == 20 && matches!(event, InputKind::Key { pressed: false, .. })));
            state.input.clear();
            state.keyboard_key(48, true);
            assert!(state.input.is_empty(), "no input after keyboard focus leaves");
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
        pub fn focused_surface(&self) -> Option<u32> {
            None
        }
        pub fn set_surface_options(&mut self, _id: u32, _enabled: bool) -> bool { false }
        pub fn contains_surface(&self, _id: u32) -> bool { false }
        pub fn is_toplevel(&self, _id: u32) -> bool { false }
        pub fn destroy_surface(&mut self, _id: u32) -> bool {
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
        pub fn present_shm(&mut self, _id: u32, _fd: std::os::fd::OwnedFd,
            _width: u32, _height: u32, _stride: u32, _format: u32, _crop: pengudroid_proto::BufferCrop) -> Result<(), String> {
            Err("graphics built without the `wayland` feature".into())
        }
        pub fn present_video_dmabuf(&mut self, _id: u32, _frame: pengudroid_hostgpu::video::ExportedFrame)
            -> Result<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>, String> {
            Err("graphics built without the wayland feature".into())
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
