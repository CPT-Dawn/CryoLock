//! DPMS idle timer — powers off/on displays via wlr-output-power-management.
//!
//! Design:
//!   - The event loop records `last_activity` on every user input (keypress).
//!   - After each dispatch round, `tick()` checks elapsed time against the
//!     configured timeout. If idle long enough, it powers off all bound outputs.
//!   - On the next keypress, `wake()` powers them back on and resets the timer.
//!   - If `dpms_timeout_seconds == 0`, DPMS blanking is disabled entirely.
//!
//! This is intentionally a simple poll-based design: the Wayland event loop
//! already wakes on keyboard input, so there is no need for a separate timer
//! thread. We pay one cheap `Instant::elapsed()` comparison per dispatch round.

use std::collections::HashMap;
use std::time::Instant;

use log::info;
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_v1::{
    self, ZwlrOutputPowerV1,
};

// ---------------------------------------------------------------------------
// DPMS state tracker
// ---------------------------------------------------------------------------

/// Tracks monitor idle state and drives power on/off transitions.
pub struct DpmsState {
    /// When the user last pressed a key (or the session was first locked).
    last_activity: Instant,
    /// Seconds of inactivity before blanking (0 = disabled).
    timeout_secs: u64,
    /// Whether monitors are currently powered off by us.
    blanked: bool,
}

impl DpmsState {
    /// Create a new DPMS state tracker.
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            last_activity: Instant::now(),
            timeout_secs,
            blanked: false,
        }
    }

    /// Record user activity — resets the idle timer.
    pub fn record_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Whether the monitors are currently blanked by us.
    #[allow(dead_code)]
    pub fn is_blanked(&self) -> bool {
        self.blanked
    }

    /// Check idle timeout and blank monitors if the threshold is exceeded.
    ///
    /// Call this after every event loop dispatch round.
    /// Returns `true` if a power state transition occurred (for logging).
    pub fn tick(&mut self, controls: &HashMap<u32, ZwlrOutputPowerV1>) -> bool {
        if self.timeout_secs == 0 || controls.is_empty() {
            return false;
        }

        let elapsed = self.last_activity.elapsed().as_secs();

        if !self.blanked && elapsed >= self.timeout_secs {
            // Idle threshold exceeded — power off all outputs.
            info!(
                "DPMS: idle for {elapsed}s ≥ {}s — blanking monitors",
                self.timeout_secs
            );
            for power in controls.values() {
                power.set_mode(zwlr_output_power_v1::Mode::Off);
            }
            self.blanked = true;
            return true;
        }

        false
    }

    /// Wake monitors back up (power on). Call on user input when blanked.
    ///
    /// Returns `true` if monitors were actually woken (were blanked before).
    pub fn wake(&mut self, controls: &HashMap<u32, ZwlrOutputPowerV1>) -> bool {
        if !self.blanked {
            return false;
        }

        info!("DPMS: user input detected — waking monitors");
        for power in controls.values() {
            power.set_mode(zwlr_output_power_v1::Mode::On);
        }
        self.blanked = false;
        self.last_activity = Instant::now();
        true
    }

    /// Ensure all monitors are powered on. Call during cleanup/unlock.
    pub fn ensure_on(&mut self, controls: &HashMap<u32, ZwlrOutputPowerV1>) {
        if self.blanked {
            for power in controls.values() {
                power.set_mode(zwlr_output_power_v1::Mode::On);
            }
            self.blanked = false;
            info!("DPMS: monitors powered on for unlock");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_is_not_blanked() {
        let state = DpmsState::new(120);
        assert!(!state.is_blanked());
    }

    #[test]
    fn disabled_when_zero_timeout() {
        let mut state = DpmsState::new(0);
        // tick with empty controls should never blank.
        let changed = state.tick(&HashMap::new());
        assert!(!changed);
        assert!(!state.is_blanked());
    }

    #[test]
    fn record_activity_resets_timer() {
        let mut state = DpmsState::new(120);
        std::thread::sleep(std::time::Duration::from_millis(10));
        state.record_activity();
        assert!(state.last_activity.elapsed().as_millis() < 50);
    }
}
