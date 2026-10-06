//! Configuration persistence with encrypted passwords.
//!
//! Account settings live in `~/.config/sufi-email/config.toml`. Passwords are
//! never stored in plaintext: they are sealed with AES-256-GCM (via `cocoon`)
//! using a key derived from a machine-specific secret, and stored as base64
//! ciphertext inside the same file. This keeps the app independent of any OS
//! keyring while ensuring the config on disk is not directly readable.
//!
//! The key source is `<config dir>/sufi-email/.secret` (created on first run,
//! chmod 600). This protects against casual reading of the config, backups
//! without the secret file, etc. It is NOT protection against an attacker who
//! compromises the user account itself — for that, use the OS keyring.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub username: String,
    /// AES-256-GCM sealed password, base64-encoded (see crypto.rs).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    /// SMTP login defaults to `username` when absent.
    #[serde(default)]
    pub smtp_username: Option<String>,
    /// SMTP password defaults to `password` when absent.
    #[serde(default)]
    pub smtp_password: Option<String>,
    /// true = STARTTLS (typically port 587); false = implicit TLS (465).
    #[serde(default)]
    pub smtp_starttls: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    /// Last window size (logical pixels), so the app reopens at the size the
    /// user left it at. Absent until the user has resized the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowSize>,
}

/// Remembered main-window size, in logical pixels (DPI-independent, so the
/// same value restores sensibly on a display with a different scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

/// Non-secret view of an account, sent to the frontend.
///
/// The sealed passwords never cross the IPC boundary: the UI only needs to
/// know whether an account is fully configured, not the ciphertext itself.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub name: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub username: String,
    /// true when the account has a password sealed on disk.
    pub has_password: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: Option<String>,
    pub smtp_starttls: bool,
}

impl From<&AccountConfig> for AccountInfo {
    fn from(a: &AccountConfig) -> Self {
        AccountInfo {
            name: a.name.clone(),
            email: a.email.clone(),
            imap_host: a.imap_host.clone(),
            imap_port: a.imap_port,
            username: a.username.clone(),
            has_password: !a.password.is_empty(),
            smtp_host: a.smtp_host.clone(),
            smtp_port: a.smtp_port,
            smtp_username: a.smtp_username.clone(),
            smtp_starttls: a.smtp_starttls,
        }
    }
}

/// Heuristic: sealed passwords are base64 (no spaces/quotes/'@' etc.) and
/// unsealable. A plaintext password that happens to be valid base64 of a
/// valid cocoon container is astronomically unlikely.
fn is_plaintext_password(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Cocoon containers start with a fixed format prefix byte; base64 of
    // that never decodes cleanly for arbitrary text. Simplest robust check:
    // try to unseal — if it fails, treat as plaintext.
    crate::crypto::unseal_password(s).is_err()
}

impl Config {
    pub fn path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    /// Config directory. Tests can override it via the SUFI_EMAIL_CONFIG_DIR
    /// env var so they never touch the user's real configuration.
    pub fn dir() -> PathBuf {
        if let Ok(override_dir) = std::env::var("SUFI_EMAIL_CONFIG_DIR") {
            return PathBuf::from(override_dir);
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("sufi-email")
    }

    pub fn load_or_default() -> Config {
        let path = Self::path();
        let mut cfg = match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::error!("failed to parse {}: {e}", path.display());
                    Config::default()
                }
            },
            // Start with no accounts; the user adds one via the UI.
            Err(_) => Config::default(),
        };

        // Drop placeholder accounts (no credentials at all) so the app never
        // tries to connect to them. Encrypted-but-empty passwords are fine.
        let before = cfg.accounts.len();
        cfg.accounts.retain(|a| !a.password.is_empty());
        if cfg.accounts.len() != before {
            log::info!(
                "removed {} placeholder account(s)",
                before - cfg.accounts.len()
            );
            let _ = cfg.save();
        }

        cfg
    }

    /// Migrate legacy plaintext passwords (pre-encryption config) to sealed
    /// form. Idempotent: already-sealed passwords fail base64+unseal checks
    /// only if they were never valid, so we detect plaintext heuristically.
    pub fn migrate_plaintext_passwords(&mut self) {
        let mut changed = false;
        for acc in &mut self.accounts {
            if is_plaintext_password(&acc.password) {
                match crate::crypto::seal_password(&acc.password) {
                    Ok(sealed) => {
                        acc.password = sealed;
                        changed = true;
                    }
                    Err(e) => log::error!("failed to seal password for '{}': {e}", acc.name),
                }
            }
            if let Some(p) = &mut acc.smtp_password {
                if is_plaintext_password(p) {
                    match crate::crypto::seal_password(p) {
                        Ok(sealed) => {
                            *p = sealed;
                            changed = true;
                        }
                        Err(e) => log::error!("failed to seal smtp password for '{}': {e}", acc.name),
                    }
                }
            }
        }
        if changed {
            log::info!("migrated plaintext passwords to encrypted storage");
            let _ = self.save();
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text =
            toml::to_string_pretty(self).map_err(|e| format!("serialize config: {e}"))?;
        fs::write(&path, text).map_err(|e| e.to_string())?;
        // 0600: the config holds mail server addresses/usernames and sealed
        // passwords; only the owner should be able to read it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
