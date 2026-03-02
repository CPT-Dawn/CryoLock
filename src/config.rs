//! CryoLock configuration system — the Embedded Asset Bootloader.
//!
//! On startup:
//!   1. The beautifully commented `default_config.toml` is baked into the binary
//!      via `include_str!`.
//!   2. We resolve `~/.config/cryolock/config.toml` using the `directories` crate.
//!   3. If the directory or file does not exist, we create them and write the
//!      embedded default to disk so the user always has a working reference.
//!   4. The file is parsed into a strict `Config` struct via `serde` + `toml`.
//!
//! Parsing failures are fatal — CryoLock will not start with a malformed config.

use std::fs;
use std::path::PathBuf;

use log::{error, info};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Embedded default config (baked into the binary at compile time)
// ---------------------------------------------------------------------------

/// The full contents of the default config file, included verbatim.
const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

// ---------------------------------------------------------------------------
// Config struct — every field is mandatory with a serde default
// ---------------------------------------------------------------------------

/// Parsed, validated configuration for CryoLock.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Seconds of inactivity before DPMS powers off monitors (0 = disabled).
    #[serde(default = "default_dpms_timeout")]
    pub dpms_timeout_seconds: u64,

    /// Lock screen background color (hex).
    #[serde(default = "default_background_color")]
    pub background_color: String,

    /// Clock and status text color (hex).
    #[serde(default = "default_text_color")]
    pub text_color: String,

    /// Ring indicator color when idle.
    #[serde(default = "default_ring_idle_color")]
    pub ring_idle_color: String,

    /// Ring indicator color while typing.
    #[serde(default = "default_ring_typing_color")]
    pub ring_typing_color: String,

    /// Ring indicator color on wrong password.
    #[serde(default = "default_ring_wrong_color")]
    pub ring_wrong_color: String,

    /// System font family name.
    #[serde(default = "default_font_family")]
    pub font_family: String,

    /// Font size in pixels for the clock display.
    #[serde(default = "default_font_size")]
    pub font_size: u32,

    /// Whether to show the clock above the ring.
    #[serde(default = "default_show_clock")]
    pub show_clock: bool,

    /// strftime-style clock format string.
    #[serde(default = "default_clock_format")]
    pub clock_format: String,
}

// -- serde default value functions ------------------------------------------

fn default_dpms_timeout() -> u64 {
    120
}
fn default_background_color() -> String {
    "#1a1b26".into()
}
fn default_text_color() -> String {
    "#c0caf5".into()
}
fn default_ring_idle_color() -> String {
    "#565f89".into()
}
fn default_ring_typing_color() -> String {
    "#7aa2f7".into()
}
fn default_ring_wrong_color() -> String {
    "#f7768e".into()
}
fn default_font_family() -> String {
    "monospace".into()
}
fn default_font_size() -> u32 {
    64
}
fn default_show_clock() -> bool {
    true
}
fn default_clock_format() -> String {
    "%H:%M".into()
}

// ---------------------------------------------------------------------------
// Color parsing helper
// ---------------------------------------------------------------------------

/// Parse a CSS hex color string (#RRGGBB or RRGGBB) into (R, G, B).
/// Returns `None` for malformed input.
pub fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Convert (R, G, B) to an ARGB8888 pixel in little-endian byte order [B, G, R, A].
#[allow(dead_code)] // Used in unit tests.
pub fn rgb_to_argb8888(r: u8, g: u8, b: u8) -> [u8; 4] {
    [b, g, r, 0xFF]
}

// ---------------------------------------------------------------------------
// Bootloader: resolve path → ensure file exists → parse
// ---------------------------------------------------------------------------

/// Resolve the config file path: `~/.config/cryolock/config.toml`.
fn config_path() -> PathBuf {
    if let Some(proj_dirs) = directories::ProjectDirs::from("", "", "cryolock") {
        proj_dirs.config_dir().join("config.toml")
    } else {
        // Fallback: use $HOME directly.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join(".config")
            .join("cryolock")
            .join("config.toml")
    }
}

/// Bootstrap the configuration file onto disk (if absent) and parse it.
///
/// # Panics
/// Panics (via `process::exit`) if the config file exists but cannot be parsed.
pub fn load() -> Config {
    let path = config_path();

    // -- Ensure directory tree exists --
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            info!("Creating config directory: {}", parent.display());
            fs::create_dir_all(parent).unwrap_or_else(|e| {
                error!(
                    "Failed to create config directory {}: {e}",
                    parent.display()
                );
                std::process::exit(1);
            });
        }
    }

    // -- Write default config if absent --
    if !path.exists() {
        info!("Writing default config to {}", path.display());
        fs::write(&path, DEFAULT_CONFIG).unwrap_or_else(|e| {
            error!("Failed to write default config to {}: {e}", path.display());
            std::process::exit(1);
        });
    }

    // -- Read and parse --
    let contents = fs::read_to_string(&path).unwrap_or_else(|e| {
        error!("Failed to read config from {}: {e}", path.display());
        std::process::exit(1);
    });

    let config: Config = toml::from_str(&contents).unwrap_or_else(|e| {
        error!(
            "Failed to parse config at {}:\n  {e}\n\nFix or delete the file to regenerate defaults.",
            path.display()
        );
        std::process::exit(1);
    });

    info!("Config loaded from {}", path.display());
    info!(
        "  DPMS timeout: {}s | clock: {} | bg: {}",
        config.dpms_timeout_seconds, config.show_clock, config.background_color
    );

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_default_parses() {
        let config: Config =
            toml::from_str(DEFAULT_CONFIG).expect("Embedded default_config.toml must parse");
        assert_eq!(config.dpms_timeout_seconds, 120);
        assert_eq!(config.background_color, "#1a1b26");
        assert!(config.show_clock);
    }

    #[test]
    fn parse_hex_colors() {
        assert_eq!(parse_hex_color("#1a1b26"), Some((0x1a, 0x1b, 0x26)));
        assert_eq!(parse_hex_color("c0caf5"), Some((0xc0, 0xca, 0xf5)));
        assert_eq!(parse_hex_color("#fff"), None); // Too short
        assert_eq!(parse_hex_color("zzzzzz"), None); // Invalid hex
    }

    #[test]
    fn argb8888_byte_order() {
        let px = rgb_to_argb8888(0x1a, 0x1b, 0x26);
        // ARGB8888 little-endian: [B, G, R, A]
        assert_eq!(px, [0x26, 0x1b, 0x1a, 0xFF]);
    }
}
