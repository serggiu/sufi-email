//! Omarchy theme integration.
//!
//! Omarchy stages the active theme at
//! `~/.local/state/omarchy/current/theme/colors.toml` (atomically swapped on
//! every `omarchy theme set`), so reading that file gives the current palette
//! for any theme — stock or custom. This module reads and parses it, serves it
//! to the frontend (which maps it onto the app's CSS variables), and watches
//! for changes so a theme switch repaints the UI live without a restart.
//!
//! When the file is absent (non-Omarchy systems), the app keeps its built-in
//! dark defaults.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tauri::Emitter;

/// The palette fields the frontend mapping consumes. Everything but the three
/// essentials is optional so a slightly-off theme still yields a coherent UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OmarchyColors {
    pub mode: String,
    pub background: String,
    pub foreground: String,
    pub accent: String,
    #[serde(default)]
    pub selection: Option<String>,
    #[serde(default)]
    pub muted: Option<String>,
    #[serde(default)]
    pub darker_background: Option<String>,
    #[serde(default)]
    pub dark_background: Option<String>,
    #[serde(default)]
    pub lighter_background: Option<String>,
    #[serde(default)]
    pub dark_foreground: Option<String>,
    #[serde(default)]
    pub light_foreground: Option<String>,
    #[serde(default)]
    pub bright_foreground: Option<String>,
    #[serde(default)]
    pub red: Option<String>,
    #[serde(default)]
    pub orange: Option<String>,
    #[serde(default)]
    pub yellow: Option<String>,
    #[serde(default)]
    pub green: Option<String>,
    #[serde(default)]
    pub cyan: Option<String>,
    #[serde(default)]
    pub blue: Option<String>,
    #[serde(default)]
    pub magenta: Option<String>,
    /// Anything else in colors.toml (bright_* variants, future fields) is
    /// tolerated and ignored rather than failing the parse.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, toml::Value>,
}

/// Path to the current Omarchy theme's palette. Tests override it via
/// `SUFI_EMAIL_THEME_FILE`; a non-Omarchy system simply has no file there.
pub fn theme_colors_path() -> PathBuf {
    if let Ok(override_path) = std::env::var("SUFI_EMAIL_THEME_FILE") {
        return PathBuf::from(override_path);
    }
    // Omarchy stages the palette under the user's home, not under
    // XDG_CONFIG_HOME. Resolve it from the home dir directly: under Flatpak the
    // config dir is remapped into ~/.var/app, so deriving the path from
    // config_dir().parent() would point into the sandbox and never see the
    // host theme. A `--filesystem` grant on ~/.local/state/omarchy exposes it.
    dirs::home_dir()
        .map(|home| home.join(".local/state/omarchy/current/theme/colors.toml"))
        .unwrap_or_else(|| PathBuf::from("colors.toml"))
}

/// Read and parse the current Omarchy palette, if one is staged.
pub fn read_theme_colors() -> Option<OmarchyColors> {
    let path = theme_colors_path();
    let text = std::fs::read_to_string(&path).ok()?;
    match toml::from_str::<OmarchyColors>(&text) {
        Ok(colors) => Some(colors),
        Err(e) => {
            log::warn!("failed to parse Omarchy theme {}: {e}", path.display());
            None
        }
    }
}

/// Serve the current palette to the frontend (None when no Omarchy theme is
/// staged — the UI then keeps its built-in defaults).
#[tauri::command]
pub fn get_system_theme() -> Option<OmarchyColors> {
    read_theme_colors()
}

/// Background loop: repolls colors.toml every few seconds and emits
/// `system-theme-changed` with the new palette whenever the file content
/// changes (a theme switch rewrites the staged dir atomically, so comparing
/// content — not inode/mtime — is what survives the swap). Emits nothing
/// while unchanged; also emits None when the file disappears so the UI can
/// fall back to its defaults.
pub fn watch(app: tauri::AppHandle) {
    let mut last: Option<String> = None;
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let content = std::fs::read_to_string(theme_colors_path()).ok();
        if content == last {
            continue;
        }
        last = content.clone();
        let payload = content
            .as_deref()
            .and_then(|c| toml::from_str::<OmarchyColors>(c).ok());
        let _ = app.emit("system-theme-changed", payload);
        // The tray envelope follows the theme too (foreground/accent fill),
        // so repaint it on every theme switch.
        crate::update_tray_icon(&app);
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;
    use std::io::Write;

    fn with_theme_file<F: FnOnce()>(colors_toml: &str, f: F) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("colors.toml");
        let mut file = std::fs::File::create(&path).expect("create colors.toml");
        file.write_all(colors_toml.as_bytes()).expect("write");
        let prev = std::env::var_os("SUFI_EMAIL_THEME_FILE");
        std::env::set_var("SUFI_EMAIL_THEME_FILE", &path);
        f();
        match prev {
            Some(v) => std::env::set_var("SUFI_EMAIL_THEME_FILE", v),
            None => std::env::remove_var("SUFI_EMAIL_THEME_FILE"),
        }
    }

    #[test]
    fn parses_a_real_omarchy_palette() {
        with_theme_file(
            "mode = \"dark\"\n\
             accent = \"#81a1c1\"\n\
             selection = \"#434c5e\"\n\
             muted = \"#4c566a\"\n\
             background = \"#2e3440\"\n\
             lighter_background = \"#3b4252\"\n\
             foreground = \"#d8dee9\"\n\
             red = \"#bf616a\"\n",
            || {
                let colors = read_theme_colors().expect("theme read");
                assert_eq!(colors.mode, "dark");
                assert_eq!(colors.background, "#2e3440");
                assert_eq!(colors.accent, "#81a1c1");
                assert_eq!(colors.selection.as_deref(), Some("#434c5e"));
                assert_eq!(colors.lighter_background.as_deref(), Some("#3b4252"));
            },
        );
    }

    #[test]
    fn tolerates_extra_and_missing_fields() {
        with_theme_file(
            "mode = \"light\"\n\
             background = \"#ffffff\"\n\
             foreground = \"#1c1e21\"\n\
             accent = \"#356fc7\"\n\
             bright_blue = \"#5a9cf0\"\n\
             custom_future_field = \"#123456\"\n",
            || {
                let colors = read_theme_colors().expect("theme read");
                assert_eq!(colors.mode, "light");
                assert_eq!(colors.selection, None); // optional, absent
                assert!(colors.extra.contains_key("bright_blue"));
            },
        );
    }

    #[test]
    fn returns_none_without_a_theme_file() {
        let prev = std::env::var_os("SUFI_EMAIL_THEME_FILE");
        std::env::set_var("SUFI_EMAIL_THEME_FILE", "/nonexistent/colors.toml");
        assert!(read_theme_colors().is_none());
        match prev {
            Some(v) => std::env::set_var("SUFI_EMAIL_THEME_FILE", v),
            None => std::env::remove_var("SUFI_EMAIL_THEME_FILE"),
        }
    }

    #[test]
    fn default_path_points_at_the_omarchy_staging_dir() {
        let prev = std::env::var_os("SUFI_EMAIL_THEME_FILE");
        std::env::remove_var("SUFI_EMAIL_THEME_FILE");
        let path = theme_colors_path();
        let s = path.to_string_lossy().to_string();
        assert!(
            s.ends_with(".local/state/omarchy/current/theme/colors.toml"),
            "unexpected theme path: {s}"
        );
        if let Some(v) = prev {
            std::env::set_var("SUFI_EMAIL_THEME_FILE", v);
        }
    }

    #[test]
    fn serialized_palette_keeps_the_field_names_the_frontend_reads() {
        with_theme_file(
            "mode = \"dark\"\n\
             background = \"#2e3440\"\n\
             foreground = \"#d8dee9\"\n\
             accent = \"#81a1c1\"\n\
             lighter_background = \"#3b4252\"\n\
             selection = \"#434c5e\"\n\
             red = \"#bf616a\"\n",
            || {
                let colors = read_theme_colors().expect("theme read");
                let json = serde_json::to_value(&colors).expect("serialize");
                let obj = json.as_object().expect("object");
                // theme.js reads exactly these keys; a rename here would
                // silently break the mapping, so pin the contract.
                for key in [
                    "mode",
                    "background",
                    "foreground",
                    "accent",
                    "lighter_background",
                    "selection",
                    "red",
                    "orange",
                ] {
                    assert!(obj.contains_key(key), "missing JSON key '{key}'");
                }
            },
        );
    }
}
