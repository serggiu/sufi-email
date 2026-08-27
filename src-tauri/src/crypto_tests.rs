//! Tests for password sealing (crypto.rs).
//!
//! The secret file location follows Config::dir(), which tests redirect via
//! the SUFI_EMAIL_CONFIG_DIR env var. Each test gets its own temp dir and
//! serializes on a global lock because the env var is process-global.

use std::sync::Mutex;

// Env vars are process-global; run crypto/config tests one at a time.
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Redirect the config dir to a temp dir for the duration of `f`.
///
/// Panic-safe: even if `f` panics, the env var is cleared and the lock is
/// recovered (a previous test's panic must not poison every later test).
pub fn with_config_dir<F: FnOnce(&std::path::Path)>(f: F) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests run single-threaded with respect to ENV_LOCK.
    std::env::set_var("SUFI_EMAIL_CONFIG_DIR", dir.path());
    struct ResetEnv;
    impl Drop for ResetEnv {
        fn drop(&mut self) {
            std::env::remove_var("SUFI_EMAIL_CONFIG_DIR");
        }
    }
    let _reset = ResetEnv;
    f(dir.path());
}

#[test]
fn seal_unseal_roundtrip() {
    with_config_dir(|_| {
        let sealed = crate::crypto::seal_password("hunter2!üñíçødé").unwrap();
        assert_ne!(sealed, "hunter2!üñíçødé");
        assert_eq!(crate::crypto::unseal_password(&sealed).unwrap(), "hunter2!üñíçødé");
    });
}

#[test]
fn sealed_output_is_deterministic_base64_but_ciphertext_differs() {
    with_config_dir(|_| {
        let a = crate::crypto::seal_password("same input").unwrap();
        let b = crate::crypto::seal_password("same input").unwrap();
        // Fresh random nonce per seal: ciphertexts differ for identical input.
        assert_ne!(a, b, "nonce reuse would be a crypto bug");
        // But both decrypt to the same plaintext.
        assert_eq!(
            crate::crypto::unseal_password(&a).unwrap(),
            crate::crypto::unseal_password(&b).unwrap()
        );
    });
}

#[test]
fn ciphertext_does_not_contain_plaintext() {
    with_config_dir(|_| {
        let secret = "super-secret-password-123";
        let sealed = crate::crypto::seal_password(secret).unwrap();
        assert!(!sealed.contains(secret));
        // Base64 alphabet has no spaces; a plaintext leak would be obvious.
        assert!(!sealed.is_empty());
    });
}

#[test]
fn unseal_with_wrong_secret_fails() {
    with_config_dir(|dir| {
        let sealed = crate::crypto::seal_password("secret").unwrap();
        // Replace the secret file: simulates a different machine / corrupted key.
        let secret_path = dir.join(".secret");
        std::fs::write(&secret_path, [7u8; 32]).unwrap();
        assert!(crate::crypto::unseal_password(&sealed).is_err());
    });
}

#[test]
fn unseal_of_garbage_fails_gracefully() {
    with_config_dir(|_| {
        assert!(crate::crypto::unseal_password("not-base64!!!").is_err());
        assert!(crate::crypto::unseal_password("").is_err());
        // Valid base64 but not a valid container.
        assert!(crate::crypto::unseal_password("aGVsbG8gd29ybGQ=").is_err());
    });
}

#[test]
fn empty_password_can_be_sealed_and_unsealed() {
    with_config_dir(|_| {
        let sealed = crate::crypto::seal_password("").unwrap();
        assert_eq!(crate::crypto::unseal_password(&sealed).unwrap(), "");
    });
}

#[test]
fn secret_file_is_created_with_0600_permissions() {
    with_config_dir(|dir| {
        crate::crypto::seal_password("trigger creation").unwrap();
        let path = dir.join(".secret");
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "secret must be owner-only");
        }
    });
}

#[test]
fn secret_file_is_reused_not_regenerated() {
    with_config_dir(|dir| {
        let sealed1 = crate::crypto::seal_password("first").unwrap();
        let secret_before = std::fs::read(dir.join(".secret")).unwrap();
        // A second seal in the same config dir must use the same secret.
        let unsealed = crate::crypto::unseal_password(&sealed1).unwrap();
        assert_eq!(unsealed, "first");
        let secret_after = std::fs::read(dir.join(".secret")).unwrap();
        assert_eq!(secret_before, secret_after, "secret must be stable");
    });
}

#[test]
fn corrupted_secret_file_is_reported_not_panic() {
    with_config_dir(|dir| {
        crate::crypto::seal_password("x").unwrap();
        std::fs::write(dir.join(".secret"), [1u8; 10]).unwrap(); // wrong size
        let sealed = crate::crypto::seal_password("y");
        assert!(sealed.is_err(), "wrong-size secret must error clearly");
    });
}
