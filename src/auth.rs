//! Secure PAM authentication wrapper for CryoLock.
//!
//! Design principles:
//!   - Password is held in a `zeroize`-backed `String` that is wiped the instant
//!     it is consumed by PAM — no copies linger in memory.
//!   - PAM authentication (`pam_authenticate`) blocks, so it runs on a dedicated
//!     thread to keep the Wayland event loop responsive.
//!   - Communication between the event loop and the auth thread uses a lock-free
//!     channel: the event loop sends a password attempt, the auth thread replies
//!     with success/failure.
//!   - The PAM service name is `cryolock` by default; falls back to `login` if
//!     `/etc/pam.d/cryolock` does not exist.

use std::sync::mpsc;
use std::thread;

use log::{error, info, warn};
use zeroize::Zeroize;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The result of a single authentication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthResult {
    /// Password was correct — unlock the session.
    Success,
    /// Password was wrong — stay locked, signal the UI.
    Failure,
    /// PAM encountered an internal error (logged, treated as failure).
    Error,
}

/// Messages sent from the event loop → auth thread.
pub enum AuthRequest {
    /// Attempt authentication with this password (will be zeroized after use).
    Attempt(String),
    /// Shut down the auth thread.
    Shutdown,
}

/// Handle for the main event loop to communicate with the auth thread.
pub struct AuthHandle {
    /// Send password attempts to the auth thread.
    tx: mpsc::Sender<AuthRequest>,
    /// Receive auth results back.
    rx: mpsc::Receiver<AuthResult>,
}

impl AuthHandle {
    /// Submit a password for authentication. The string will be zeroized by the
    /// auth thread after use — the caller should also zeroize their copy.
    pub fn try_authenticate(&self, password: String) {
        if self.tx.send(AuthRequest::Attempt(password)).is_err() {
            error!("Auth thread has terminated — cannot authenticate");
        }
    }

    /// Non-blocking poll for an authentication result.
    /// Returns `None` if no result is ready yet.
    pub fn poll_result(&self) -> Option<AuthResult> {
        self.rx.try_recv().ok()
    }

    /// Signal the auth thread to exit. Best-effort; safe to ignore errors.
    pub fn shutdown(&self) {
        let _ = self.tx.send(AuthRequest::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// Auth thread logic
// ---------------------------------------------------------------------------

/// Resolve the current user's login name.
fn current_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| {
            // Fall back to libc getuid → getpwuid.
            let uid = unsafe { libc::getuid() };
            let pw = unsafe { libc::getpwuid(uid) };
            if pw.is_null() {
                "root".into()
            } else {
                let cstr = unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) };
                cstr.to_string_lossy().into_owned()
            }
        })
}

/// Determine which PAM service to use.
fn pam_service() -> &'static str {
    if std::path::Path::new("/etc/pam.d/cryolock").exists() {
        "cryolock"
    } else {
        // Common fallback on most Linux distros.
        info!("PAM service 'cryolock' not found, falling back to 'login'");
        "login"
    }
}

/// Perform a single PAM authentication attempt.
/// The `password` is zeroized before this function returns.
fn authenticate_once(username: &str, mut password: String) -> AuthResult {
    let service = pam_service();

    // Create a PAM client with the built-in PasswordConv handler.
    let mut client = match pam::Client::with_password(service) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create PAM client (service={service}): {e}");
            password.zeroize();
            return AuthResult::Error;
        }
    };

    // Supply credentials to the conversation handler.
    client
        .conversation_mut()
        .set_credentials(username, &password);

    // Zeroize the password immediately — PAM has copied it internally.
    password.zeroize();

    // Block on pam_authenticate + pam_acct_mgmt.
    match client.authenticate() {
        Ok(()) => {
            info!("PAM authentication succeeded");
            AuthResult::Success
        }
        Err(e) => {
            warn!("PAM authentication failed: {e}");
            AuthResult::Failure
        }
    }
}

/// The auth thread's main loop. Waits for `AuthRequest`s and replies with
/// `AuthResult`s. Exits when it receives `Shutdown` or the channel closes.
fn auth_thread_main(rx: mpsc::Receiver<AuthRequest>, tx: mpsc::Sender<AuthResult>) {
    let username = current_username();
    info!("Auth thread started for user '{username}'");

    loop {
        match rx.recv() {
            Ok(AuthRequest::Attempt(password)) => {
                let result = authenticate_once(&username, password);
                if tx.send(result).is_err() {
                    // Main thread dropped its receiver — exit.
                    break;
                }
            }
            Ok(AuthRequest::Shutdown) | Err(_) => {
                info!("Auth thread shutting down");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Spawn the authentication thread and return a handle for the event loop.
pub fn spawn() -> AuthHandle {
    let (req_tx, req_rx) = mpsc::channel::<AuthRequest>();
    let (res_tx, res_rx) = mpsc::channel::<AuthResult>();

    thread::Builder::new()
        .name("cryolock-auth".into())
        .spawn(move || auth_thread_main(req_rx, res_tx))
        .expect("Failed to spawn auth thread");

    AuthHandle {
        tx: req_tx,
        rx: res_rx,
    }
}
