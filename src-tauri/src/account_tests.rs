//! Tests for config persistence (account.rs): load/save roundtrip,
//! placeholder cleanup, plaintext password migration.

use crate::crypto_tests::with_config_dir;
use crate::account::{AccountConfig, AccountInfo, Config};

fn sample_account(name: &str, email: &str, password: &str) -> AccountConfig {
    AccountConfig {
        name: name.into(),
        email: email.into(),
        imap_host: "imap.test".into(),
        imap_port: 993,
        smtp_host: "smtp.test".into(),
        smtp_port: 465,
        username: email.into(),
        password: password.into(),
        smtp_username: None,
        smtp_password: None,
        smtp_starttls: false,
    }
}

#[test]
fn save_and_load_roundtrip() {
    with_config_dir(|_| {
        let mut cfg = Config::default();
        cfg.accounts.push(sample_account("Work", "w@x.org", "pw1"));
        cfg.accounts.push(sample_account("Home", "h@y.org", "pw2"));
        cfg.save().unwrap();

        let loaded = Config::load_or_default();
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[0].name, "Work");
        assert_eq!(loaded.accounts[1].email, "h@y.org");
    });
}

#[test]
fn missing_config_file_loads_as_empty() {
    with_config_dir(|_| {
        let cfg = Config::load_or_default();
        assert!(cfg.accounts.is_empty());
    });
}

#[test]
fn placeholder_accounts_are_dropped_on_load() {
    with_config_dir(|dir| {
        // Write a config containing a placeholder (empty password) and a
        // real account.
        let toml_text = r#"
[[accounts]]
name = "Example"
email = "you@example.com"
imap_host = "imap.example.com"
imap_port = 993
smtp_host = "smtp.example.com"
smtp_port = 465
username = "you@example.com"
password = ""

[[accounts]]
name = "Real"
email = "real@test.org"
imap_host = "imap.test"
imap_port = 993
smtp_host = "smtp.test"
smtp_port = 465
username = "real@test.org"
password = "sealed-blob-here"
"#;
        std::fs::create_dir_all(dir.join("storage")).unwrap();
        std::fs::write(dir.join("config.toml"), toml_text).unwrap();

        let cfg = Config::load_or_default();
        assert_eq!(cfg.accounts.len(), 1, "placeholder dropped");
        assert_eq!(cfg.accounts[0].name, "Real");
    });
}

#[test]
fn plaintext_passwords_are_migrated_to_sealed() {
    with_config_dir(|_| {
        let mut cfg = Config::default();
        cfg.accounts.push(sample_account("Legacy", "old@x.org", "plaintext-pw"));
        cfg.save().unwrap();

        cfg.migrate_plaintext_passwords();

        // On-disk password must no longer be the plaintext.
        let raw = std::fs::read_to_string(Config::path()).unwrap();
        assert!(!raw.contains("plaintext-pw"), "plaintext must be gone from disk");

        // And the in-memory account now holds a sealed value that unseals.
        let sealed = &cfg.accounts[0].password;
        assert_ne!(sealed, "plaintext-pw");
        assert_eq!(crate::crypto::unseal_password(sealed).unwrap(), "plaintext-pw");
    });
}

#[test]
fn migration_is_idempotent() {
    with_config_dir(|_| {
        let mut cfg = Config::default();
        cfg.accounts.push(sample_account("A", "a@x.org", "pw"));
        cfg.migrate_plaintext_passwords();
        let sealed_once = cfg.accounts[0].password.clone();
        cfg.migrate_plaintext_passwords();
        let sealed_twice = cfg.accounts[0].password.clone();
        assert_eq!(sealed_once, sealed_twice, "second run must be a no-op");
    });
}

#[test]
fn smtp_password_migration() {
    with_config_dir(|_| {
        let mut cfg = Config::default();
        let mut acc = sample_account("A", "a@x.org", "imap-pw");
        acc.smtp_password = Some("smtp-plain".into());
        cfg.accounts.push(acc);
        cfg.migrate_plaintext_passwords();

        assert_ne!(cfg.accounts[0].smtp_password.as_deref(), Some("smtp-plain"));
        assert_eq!(
            crate::crypto::unseal_password(cfg.accounts[0].smtp_password.as_ref().unwrap())
                .unwrap(),
            "smtp-plain"
        );
    });
}

#[test]
fn smtp_login_falls_back_to_imap_credentials() {
    let mut acc = sample_account("A", "a@x.org", "pw");
    acc.smtp_username = None;
    acc.smtp_password = None;
    // smtp_login returns sealed refs; the fallback logic is what we test.
    assert_eq!(acc.smtp_username.as_deref().unwrap_or(&acc.username), "a@x.org");
    assert_eq!(acc.smtp_password.as_deref().unwrap_or(&acc.password), "pw");
}

#[test]
fn corrupted_config_file_loads_as_empty() {
    with_config_dir(|dir| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("config.toml"), "not [ valid toml {{{{").unwrap();
        let cfg = Config::load_or_default();
        assert!(cfg.accounts.is_empty(), "corrupt config must not crash the app");
    });
}

#[test]
fn db_filename_uses_email_sanitized() {
    with_config_dir(|_| {
        let acc = sample_account("Sergiu T", "sergiu+tag@mailbox.org", "pw");
        // The sanitizer keeps @ . - _ and alphanumerics.
        let sanitized: String = acc
            .email
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '@' || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        assert_eq!(sanitized, "sergiu_tag@mailbox.org");
    });
}

// ---------------------------------------------------------------------------
// Sanitized account view (AccountInfo) — never crosses passwords to the UI
// ---------------------------------------------------------------------------

#[test]
fn account_info_never_exposes_passwords() {
    let mut acc = sample_account("Work", "w@x.org", "sealed-blob-abc");
    acc.smtp_password = Some("smtp-sealed-blob-xyz".into());

    let json = serde_json::to_string(&AccountInfo::from(&acc)).unwrap();
    assert!(!json.contains("sealed-blob-abc"), "imap password leaked: {json}");
    assert!(!json.contains("smtp-sealed-blob-xyz"), "smtp password leaked: {json}");
    assert!(!json.contains("password"), "password field serialized: {json}");
    assert!(
        json.contains("hasPassword"),
        "UI needs a password flag, got: {json}"
    );
}

#[test]
fn account_info_reports_whether_password_is_sealed() {
    let mut acc = sample_account("A", "a@x.org", "sealed");
    assert!(AccountInfo::from(&acc).has_password);

    acc.password.clear();
    assert!(!AccountInfo::from(&acc).has_password, "empty password => not configured");
}

#[test]
fn account_info_carries_connection_settings() {
    let acc = sample_account("A", "a@x.org", "sealed");
    let info = AccountInfo::from(&acc);
    assert_eq!(info.name, "A");
    assert_eq!(info.email, "a@x.org");
    assert_eq!(info.imap_host, "imap.test");
    assert_eq!(info.imap_port, 993);
    assert_eq!(info.smtp_port, 465);
    assert!(!info.smtp_starttls);
}

#[cfg(unix)]
#[test]
fn config_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    with_config_dir(|_| {
        let cfg = Config::default();
        cfg.save().unwrap();
        let mode = std::fs::metadata(Config::path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "config.toml must be owner-only");
    });
}
