//! CryoLock — a blazing-fast, secure Wayland session locker.
//!
//! Entry point: connects to the Wayland display, registers all protocol globals,
//! acquires the ext-session-lock-v1 lock, and runs the blocking event loop.

mod app;
mod auth;
mod config;
mod dpms;
mod render;

use log::{error, info};
use wayland_client::{globals::registry_queue_init, Connection};

use crate::app::CryoLock;

fn main() {
    // Initialise logging (RUST_LOG=info by default for diagnostics).
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("CryoLock v{} starting", env!("CARGO_PKG_VERSION"));

    // ── Load configuration (bootloader pattern) ───────────────────────────
    // This must happen BEFORE the Wayland connection so that a malformed
    // config exits cleanly without seizing the compositor.
    let config = config::load();

    // ── Connect to the Wayland display ────────────────────────────────────
    let conn = Connection::connect_to_env().unwrap_or_else(|e| {
        error!("Cannot connect to Wayland display: {e}");
        std::process::exit(1);
    });

    // ── Round-trip: discover all globals ──────────────────────────────────
    let (globals, mut event_queue) = registry_queue_init::<CryoLock>(&conn).unwrap_or_else(|e| {
        error!("Failed to initialise Wayland registry: {e}");
        std::process::exit(1);
    });
    let qh = event_queue.handle();

    // ── Build application state ──────────────────────────────────────────
    let mut app = CryoLock::new(&globals, &qh, config);

    // ── Request the session lock ─────────────────────────────────────────
    app.lock(&qh);

    // ── Blocking event loop ──────────────────────────────────────────────
    info!("Entering event loop");
    while app.running {
        if let Err(e) = event_queue.blocking_dispatch(&mut app) {
            error!("Wayland dispatch error: {e}");
            break;
        }
        // Check for authentication results after each dispatch round.
        app.poll_auth();
        // Check DPMS idle timeout.
        app.tick_dpms();
    }

    // \u{2500}\u{2500} Cleanup \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
    // Ensure monitors are powered on before unlocking.
    app.dpms_state.ensure_on(&app.dpms_controls);

    // Signal the auth thread to shut down.
    app.auth_handle.shutdown();

    // Ensure the compositor processes unlock_and_destroy before we exit.
    if let Some(ref lock) = app.session_lock {
        if lock.is_locked() {
            lock.unlock();
            info!("Session unlocked");
        }
    }
    let _ = conn.roundtrip();
    info!("CryoLock exiting");
}
