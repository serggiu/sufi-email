//! Password sealing for config storage.
//!
//! Passwords are encrypted with AES-256-GCM (authenticated encryption) using
//! a key derived from a machine-local random secret. Ciphertext is stored
//! base64-encoded in the TOML config, so the file remains portable TOML while
//! the passwords in it are unreadable without the secret file.
//!
//! Threat model: this prevents plaintext password exposure from the config
//! file alone (backups, dotfile sync, casual snooping). It does not protect
//! against an attacker with code execution as your user — they could read the
//! secret file too. For hard binding to the OS user, use the system keyring.

use cocoon::Cocoon;
use std::fs;
use std::path::PathBuf;

const SECRET_FILE: &str = ".secret";
// 32 bytes = AES-256 key material.
const SECRET_LEN: usize = 32;

/// Path of the machine-local secret used to derive the encryption key.
fn secret_path() -> PathBuf {
    crate::account::Config::dir().join(SECRET_FILE)
}

/// Load (or create) the machine-local secret and return a Cocoon cipher.
fn cocoon() -> Result<Cocoon<'static, cocoon::Creation>, String> {
    let path = secret_path();
    let secret: [u8; SECRET_LEN] = match fs::read(&path) {
        Ok(bytes) if bytes.len() == SECRET_LEN => bytes
            .try_into()
            .expect("length checked above"),
        Ok(bytes) if bytes.is_empty() || bytes.len() != SECRET_LEN => {
            return Err(format!(
                "secret file {} has wrong size ({} bytes, expected {SECRET_LEN})",
                path.display(),
                bytes.len()
            ));
        }
        Ok(_) => unreachable!(),
        Err(_) => {
            // First run: generate and persist with owner-only permissions.
            let mut key = [0u8; SECRET_LEN];
            getrandom::fill(&mut key).map_err(|e| format!("generate secret: {e}"))?;
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            fs::write(&path, key).map_err(|e| format!("write secret: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                    .map_err(|e| format!("restrict secret permissions: {e}"))?;
            }
            key
        }
    };
    // The stored secret is already 32 bytes of CSPRNG output, so we skip
    // the extra KDF pass (Cocoon's default PBKDF2 targets human passwords).
    let leaked: &'static [u8] = Box::leak(secret.to_vec().into_boxed_slice());
    Ok(Cocoon::new(leaked))
}

/// Encrypt a password; returns base64 ciphertext to store in the config.
pub fn seal_password(plain: &str) -> Result<String, String> {
    let mut cocoon = cocoon()?;
    let sealed = cocoon
        .wrap(plain.as_bytes())
        .map_err(|e| format!("seal password: {e:?}"))?;
    Ok(base64_encode(&sealed))
}

/// Decrypt a password previously sealed by [`seal_password`].
pub fn unseal_password(sealed_b64: &str) -> Result<String, String> {
    let cocoon = cocoon()?;
    let data = base64_decode(sealed_b64)?;
    let plain = cocoon
        .unwrap(&data)
        .map_err(|e| format!("unseal password: {e:?} (wrong/corrupt .secret?)"))?;
    String::from_utf8(plain).map_err(|e| format!("password not utf-8: {e}"))
}

// --- minimal standard-library base64 (no extra dependency) ---

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u32, String> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' | b'-' => Ok(62),
            b'/' | b'_' => Ok(63),
            _ => Err(format!("invalid base64 character '{}'", c as char)),
        }
    }
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|b| !b" \r\n\t".contains(b))
        .take_while(|&b| b != b'=')
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}
