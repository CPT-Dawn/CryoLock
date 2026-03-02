//! CryoLock application state and Wayland protocol handlers.
//!
//! This module defines the central `CryoLock` struct that holds all SCTK state,
//! implements every required handler trait, and manages the session lock lifecycle.

use std::collections::HashMap;

use log::{error, info, warn};

use zeroize::Zeroize;

use crate::auth::{self, AuthHandle, AuthResult};
use crate::config::Config;
use crate::dpms::DpmsState;
use crate::render::{InputState, Renderer};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat,
    delegate_session_lock, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        Capability, SeatHandler, SeatState,
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::GlobalList,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};

// ---------------------------------------------------------------------------
// Per-output lock surface entry
// ---------------------------------------------------------------------------

/// Holds the lock surface and associated rendering state for a single output.
pub struct LockSurfaceEntry {
    pub lock_surface: SessionLockSurface,
    pub output: wl_output::WlOutput,
    pub width: u32,
    pub height: u32,
}

// ---------------------------------------------------------------------------
// Main application state
// ---------------------------------------------------------------------------

/// Central state struct for CryoLock. Owns all SCTK subsystem states and the
/// session lock handle. Passed as `&mut self` into every Wayland event callback.
pub struct CryoLock {
    // -- SCTK core --
    pub registry_state: RegistryState,
    pub output_state: OutputState,
    pub seat_state: SeatState,
    pub compositor_state: CompositorState,
    pub shm_state: Shm,
    pub pool: SlotPool,

    // -- Session lock --
    pub session_lock_state: SessionLockState,
    pub session_lock: Option<SessionLock>,
    pub lock_surfaces: Vec<LockSurfaceEntry>,
    pub locked: bool,

    // -- DPMS (wlr-output-power-management) --
    /// The global manager. `None` if the compositor does not advertise the protocol.
    pub dpms_manager: Option<ZwlrOutputPowerManagerV1>,
    /// Per-output power controls keyed by the output's `WlOutput` id.
    pub dpms_controls: HashMap<u32, ZwlrOutputPowerV1>,

    // -- Configuration --
    pub config: Config,

    // -- Authentication --
    pub auth_handle: AuthHandle,

    // -- Rendering & input --
    pub renderer: Renderer,
    pub input_state: InputState,
    pub password_buf: String,

    // -- DPMS idle tracking --
    pub dpms_state: DpmsState,

    // -- Event-loop control --
    pub running: bool,
}

impl CryoLock {
    /// Construct CryoLock from an already-connected global list.
    pub fn new(globals: &GlobalList, qh: &QueueHandle<Self>, config: Config) -> Self {
        // Spawn the PAM authentication thread.
        let auth_handle = auth::spawn();

        // Initialise the renderer (loads system font).
        let renderer = Renderer::new(&config.font_family);
        // Bind SCTK subsystems ---------------------------------------------
        let registry_state = RegistryState::new(globals);
        let output_state = OutputState::new(globals, qh);
        let seat_state = SeatState::new(globals, qh);
        let compositor_state =
            CompositorState::bind(globals, qh).expect("wl_compositor not available");
        let shm_state = Shm::bind(globals, qh).expect("wl_shm not available");
        let session_lock_state = SessionLockState::new(globals, qh);

        // SHM pool for pixel buffers (start 256 KiB — grows automatically)
        let pool = SlotPool::new(256 * 1024, &shm_state).expect("Failed to create SHM pool");

        // Try to bind the DPMS global (optional — not all compositors support it)
        let dpms_manager: Option<ZwlrOutputPowerManagerV1> = globals
            .bind::<ZwlrOutputPowerManagerV1, _, _>(qh, 1..=1, ())
            .ok();
        if dpms_manager.is_some() {
            info!("Bound zwlr_output_power_manager_v1");
        } else {
            warn!("zwlr_output_power_manager_v1 not available — DPMS disabled");
        }

        let dpms_timeout = config.dpms_timeout_seconds;

        Self {
            registry_state,
            output_state,
            seat_state,
            compositor_state,
            shm_state,
            pool,
            session_lock_state,
            session_lock: None,
            lock_surfaces: Vec::new(),
            locked: false,
            dpms_manager,
            dpms_controls: HashMap::new(),
            config,
            auth_handle,
            renderer,
            input_state: InputState::Idle,
            password_buf: String::new(),
            dpms_state: DpmsState::new(dpms_timeout),
            running: true,
        }
    }

    /// Acquire the session lock from the compositor.
    pub fn lock(&mut self, qh: &QueueHandle<Self>) {
        match self.session_lock_state.lock(qh) {
            Ok(lock) => {
                info!("Session lock requested");
                self.session_lock = Some(lock);
            }
            Err(e) => {
                error!("Failed to acquire session lock: {e}");
                self.running = false;
            }
        }
    }

    /// Create a lock surface for the given output and push it onto our list.
    fn create_lock_surface_for_output(
        &mut self,
        output: &wl_output::WlOutput,
        qh: &QueueHandle<Self>,
    ) {
        let Some(ref session_lock) = self.session_lock else {
            return;
        };
        let wl_surface = self.compositor_state.create_surface(qh);
        let lock_surface = session_lock.create_lock_surface(wl_surface, output, qh);
        self.lock_surfaces.push(LockSurfaceEntry {
            lock_surface,
            output: output.clone(),
            width: 0,
            height: 0,
        });
    }

    /// Bind DPMS power control for a single output (if the manager exists).
    fn bind_dpms_for_output(&mut self, output: &wl_output::WlOutput, qh: &QueueHandle<Self>) {
        if let Some(ref manager) = self.dpms_manager {
            let power = manager.get_output_power(output, qh, ());
            let id = output.id().protocol_id();
            self.dpms_controls.insert(id, power);
        }
    }

    /// Render the full lock screen UI to the given lock surface entry.
    fn render_frame(&mut self, index: usize) {
        let entry = &self.lock_surfaces[index];
        let width = entry.width;
        let height = entry.height;
        if width == 0 || height == 0 {
            return;
        }

        let stride = width as i32 * 4; // ARGB8888, 4 bytes per pixel
        let buf_size = (height as i32 * stride) as usize;

        // Ensure pool has enough space
        if self.pool.len() < buf_size {
            self.pool
                .resize(buf_size)
                .expect("Failed to resize SHM pool");
        }

        match self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok((buffer, canvas)) => {
                // Delegate to the full rendering pipeline.
                self.renderer.render_frame(
                    canvas,
                    width,
                    height,
                    &self.config,
                    self.input_state,
                    self.password_buf.len(),
                );

                let entry = &self.lock_surfaces[index];
                let wl_surface = entry.lock_surface.wl_surface();
                buffer
                    .attach_to(wl_surface)
                    .expect("Failed to attach buffer");
                wl_surface.damage_buffer(0, 0, width as i32, height as i32);
                wl_surface.commit();
            }
            Err(e) => {
                error!("Failed to create SHM buffer: {e}");
            }
        }
    }

    /// Re-render all lock surfaces (e.g. after state change).
    fn render_all_surfaces(&mut self) {
        for i in 0..self.lock_surfaces.len() {
            self.render_frame(i);
        }
    }

    /// Process a single key press: buffer characters, submit on Enter, etc.
    fn handle_key(&mut self, event: &KeyEvent) {
        // Record user activity for DPMS idle tracking.
        self.dpms_state.record_activity();

        // If monitors are blanked, wake them on any keypress and swallow the key.
        if self.dpms_state.wake(&self.dpms_controls) {
            return;
        }

        match self.input_state {
            InputState::Verifying => {
                // Ignore all input while PAM is working.
                return;
            }
            InputState::Wrong => {
                // Any key clears the wrong state.
                self.password_buf.zeroize();
                self.password_buf.clear();
                // If it's a printable character, start a new attempt.
                if let Some(ref text) = event.utf8 {
                    if !text.is_empty() && !text.chars().all(|c| c.is_control()) {
                        self.password_buf.push_str(text);
                        self.input_state = InputState::Typing;
                        self.render_all_surfaces();
                        return;
                    }
                }
                self.input_state = InputState::Idle;
                self.render_all_surfaces();
                return;
            }
            _ => {}
        }

        let keysym = event.keysym;

        if keysym == Keysym::Return || keysym == Keysym::KP_Enter {
            // Submit password to auth thread.
            if !self.password_buf.is_empty() {
                let password = self.password_buf.clone();
                self.password_buf.zeroize();
                self.password_buf.clear();
                self.auth_handle.try_authenticate(password);
                self.input_state = InputState::Verifying;
                self.render_all_surfaces();
            }
        } else if keysym == Keysym::BackSpace {
            self.password_buf.pop();
            if self.password_buf.is_empty() {
                self.input_state = InputState::Idle;
            }
            self.render_all_surfaces();
        } else if keysym == Keysym::Escape {
            self.password_buf.zeroize();
            self.password_buf.clear();
            self.input_state = InputState::Idle;
            self.render_all_surfaces();
        } else if let Some(ref text) = event.utf8 {
            // Printable character.
            if !text.is_empty() && !text.chars().all(|c| c.is_control()) {
                self.password_buf.push_str(text);
                self.input_state = InputState::Typing;
                self.render_all_surfaces();
            }
        }
    }

    /// Non-blocking poll for authentication results. Call after each dispatch.
    pub fn poll_auth(&mut self) {
        if let Some(result) = self.auth_handle.poll_result() {
            match result {
                AuthResult::Success => {
                    info!("Authentication successful — unlocking session"); // Ensure monitors are on before we exit.
                    self.dpms_state.ensure_on(&self.dpms_controls);
                    self.running = false;
                }
                AuthResult::Failure | AuthResult::Error => {
                    info!("Authentication failed — showing wrong indicator");
                    self.input_state = InputState::Wrong;
                    self.password_buf.zeroize();
                    self.password_buf.clear();
                    self.render_all_surfaces();
                }
            }
        }
    }

    /// Tick the DPMS idle timer. Call after each dispatch round.
    pub fn tick_dpms(&mut self) {
        self.dpms_state.tick(&self.dpms_controls);
    }
}

// ===========================================================================
// SCTK handler trait implementations
// ===========================================================================

// -- Session Lock -----------------------------------------------------------

impl SessionLockHandler for CryoLock {
    fn locked(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, _session_lock: SessionLock) {
        info!("Session locked — creating lock surfaces for all outputs");
        self.locked = true;

        // Create lock surfaces for every known output.
        let outputs: Vec<wl_output::WlOutput> = self.output_state.outputs().collect();
        for output in &outputs {
            self.create_lock_surface_for_output(output, qh);
            self.bind_dpms_for_output(output, qh);
        }
    }

    fn finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session_lock: SessionLock,
    ) {
        error!("Compositor rejected or ended the session lock");
        self.running = false;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        info!("Lock surface configure: {w}x{h}");

        // Find the matching entry and update dimensions.
        let index = self
            .lock_surfaces
            .iter()
            .position(|e| e.lock_surface.wl_surface().id() == surface.wl_surface().id());

        if let Some(idx) = index {
            self.lock_surfaces[idx].width = w;
            self.lock_surfaces[idx].height = h;

            // Ack the configure before committing.
            surface.wl_surface().commit(); // ack_configure is implicit via SCTK

            // Render and present.
            self.render_frame(idx);
        }
    }
}

// -- Output -----------------------------------------------------------------

impl OutputHandler for CryoLock {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        info!("New output detected");
        // If we are already locked, immediately create a lock surface for the new output.
        if self.locked {
            self.create_lock_surface_for_output(&output, qh);
            self.bind_dpms_for_output(&output, qh);
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        // Output mode/geometry changed — the compositor will send a new configure.
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        info!("Output removed");
        // Remove the lock surface associated with this output.
        self.lock_surfaces.retain(|e| e.output.id() != output.id());
        // Remove DPMS control.
        let id = output.id().protocol_id();
        if let Some(power) = self.dpms_controls.remove(&id) {
            power.destroy();
        }
    }
}

// -- Compositor -------------------------------------------------------------

impl CompositorHandler for CryoLock {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // Handled via configure events on lock surfaces.
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

// -- Seat & Keyboard --------------------------------------------------------

impl SeatHandler for CryoLock {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        // When the seat gains a keyboard, request keyboard events.
        if capability == Capability::Keyboard {
            info!("Keyboard capability detected — requesting keyboard");
            let _ = self.seat_state.get_keyboard(qh, &seat, None);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl KeyboardHandler for CryoLock {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(&event);
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        // Repeats behave identically to press for our purposes.
        self.handle_key(&event);
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }
}

// -- SHM --------------------------------------------------------------------

impl ShmHandler for CryoLock {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

// -- DPMS (raw wayland-client Dispatch) -------------------------------------
// These are not covered by SCTK; we implement Dispatch manually.

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for CryoLock {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrOutputPowerManagerV1,
        _event: zwlr_output_power_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ZwlrOutputPowerV1, ()> for CryoLock {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_power_v1::Event::Mode { mode } => {
                info!("DPMS mode event: {mode:?}");
            }
            zwlr_output_power_v1::Event::Failed => {
                warn!("DPMS control failed for an output — destroying control");
                _proxy.destroy();
            }
            _ => {}
        }
    }
}

// -- Registry ---------------------------------------------------------------

impl ProvidesRegistryState for CryoLock {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

// ===========================================================================
// SCTK delegation macros — wire up Dispatch impls automatically
// ===========================================================================

delegate_registry!(CryoLock);
delegate_output!(CryoLock);
delegate_seat!(CryoLock);
delegate_keyboard!(CryoLock);
delegate_shm!(CryoLock);
delegate_compositor!(CryoLock);
delegate_session_lock!(CryoLock);
