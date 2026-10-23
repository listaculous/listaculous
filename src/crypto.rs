//! Encryption at rest for stored secrets (email provider passwords, AWS and DNS API keys).
//!
//! Values are AES-256-GCM encrypted and stored as `enc:v1:<base64(nonce || ciphertext)>`. The key
//! comes from `LISTACULOUS_SECRET_KEY` (32 bytes, base64). If unset and the database is SQLite, a
//! key file is generated next to the database on first start; Postgres deployments must set it.

use std::sync::OnceLock;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, bail, Context};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::Value;

const PREFIX: &str = "enc:v1:";
static CIPHER: OnceLock<Aes256Gcm> = OnceLock::new();

/// JSON keys whose values are secrets wherever they appear in a stored config.
pub const SECRET_FIELDS: &[&str] = &["secret_access_key", "password", "api_key", "secret_api_key"];

pub fn init(database_url: &str) -> anyhow::Result<()> {
    let key = match std::env::var("LISTACULOUS_SECRET_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(k) => B64
            .decode(k.trim())
            .context("LISTACULOUS_SECRET_KEY must be base64")?,
        None => key_file(database_url)?,
    };
    if key.len() != 32 {
        bail!("LISTACULOUS_SECRET_KEY must decode to 32 bytes (generate one with `openssl rand -base64 32`)");
    }
    let _ = CIPHER.set(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)));
    Ok(())
}

/// For SQLite installs: a key file beside the database, created on first start. (It protects
/// secrets in DB copies/backups that don't include the key file; for stronger separation set
/// LISTACULOUS_SECRET_KEY from your secret store.)
fn key_file(database_url: &str) -> anyhow::Result<Vec<u8>> {
    if !database_url.starts_with("sqlite:") {
        bail!("set LISTACULOUS_SECRET_KEY (32 random bytes, base64) when using Postgres");
    }
    let path = database_url
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");
    let path = path.split('?').next().unwrap_or("");
    let dir = std::path::Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let file = dir.join("secret.key");
    if let Ok(s) = std::fs::read_to_string(&file) {
        return B64.decode(s.trim()).context("reading secret.key");
    }
    let key = Aes256Gcm::generate_key(OsRng);
    std::fs::create_dir_all(dir)?;
    write_private(&file, B64.encode(key).as_bytes())?;
    tracing::warn!(
        "generated encryption key at {} — back it up with your database",
        file.display()
    );
    Ok(key.to_vec())
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(data)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

fn cipher() -> anyhow::Result<&'static Aes256Gcm> {
    CIPHER
        .get()
        .ok_or_else(|| anyhow!("encryption not initialised"))
}

pub fn is_encrypted(s: &str) -> bool {
    s.starts_with(PREFIX)
}

pub fn encrypt(plain: &str) -> anyhow::Result<String> {
    let nonce = Aes256Gcm::generate_nonce(OsRng);
    let ct = cipher()?
        .encrypt(&nonce, plain.as_bytes())
        .map_err(|_| anyhow!("encryption failed"))?;
    let mut buf = nonce.to_vec();
    buf.extend_from_slice(&ct);
    Ok(format!("{PREFIX}{}", B64.encode(buf)))
}

/// Decrypts an `enc:v1:` value; plain values (stored before encryption existed) pass through.
pub fn decrypt(value: &str) -> anyhow::Result<String> {
    let Some(b64) = value.strip_prefix(PREFIX) else {
        return Ok(value.to_string());
    };
    let buf = B64.decode(b64).context("corrupt encrypted value")?;
    if buf.len() < 12 {
        bail!("corrupt encrypted value");
    }
    let (nonce, ct) = buf.split_at(12);
    let pt = cipher()?
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| {
            anyhow!("can't decrypt a stored secret — was LISTACULOUS_SECRET_KEY changed?")
        })?;
    Ok(String::from_utf8(pt)?)
}

/// Encrypts every non-empty secret field of a JSON config object (already-encrypted ones are kept).
pub fn seal_config(v: &mut Value) -> anyhow::Result<()> {
    if let Some(obj) = v.as_object_mut() {
        for k in SECRET_FIELDS {
            if let Some(Value::String(s)) = obj.get_mut(*k) {
                if !s.is_empty() && !is_encrypted(s) {
                    *s = encrypt(s)?;
                }
            }
        }
    }
    Ok(())
}

/// Returns a copy of a stored config with secret fields decrypted, for actual use.
pub fn open_config(stored: &str) -> anyhow::Result<Value> {
    let mut v: Value =
        serde_json::from_str(stored).unwrap_or_else(|_| Value::Object(Default::default()));
    if let Some(obj) = v.as_object_mut() {
        for k in SECRET_FIELDS {
            if let Some(Value::String(s)) = obj.get_mut(*k) {
                *s = decrypt(s)?;
            }
        }
    }
    Ok(v)
}

/// Encrypts secrets that were stored in plain text by earlier versions.
pub async fn migrate_plaintext(db: &crate::db::Db) -> anyhow::Result<()> {
    let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, provider_config FROM sites")
        .fetch_all(db)
        .await?;
    for (id, cfg) in rows {
        let mut v: Value = serde_json::from_str(&cfg).unwrap_or_default();
        let before = v.clone();
        seal_config(&mut v)?;
        if v != before {
            // Only if unchanged since we read it (another server may be editing it right now);
            // a changed row is sealed by that save instead.
            let r = sqlx::query(
                "UPDATE sites SET provider_config = $1 WHERE id = $2 AND provider_config = $3",
            )
            .bind(v.to_string())
            .bind(id)
            .bind(&cfg)
            .execute(db)
            .await?;
            if r.rows_affected() == 1 {
                tracing::info!("encrypted stored provider secrets for site {id}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub fn init_for_tests() {
    let _ = CIPHER.set(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&[7u8; 32])));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_config_sealing() {
        init_for_tests();
        let e = encrypt("hunter2").unwrap();
        assert!(e.starts_with(PREFIX) && !e.contains("hunter2"));
        assert_eq!(decrypt(&e).unwrap(), "hunter2");
        assert_eq!(decrypt("legacy-plain").unwrap(), "legacy-plain");
        assert_ne!(
            encrypt("x").unwrap(),
            encrypt("x").unwrap(),
            "nonces must differ"
        );

        let mut cfg = serde_json::json!({ "region": "us-east-1", "access_key_id": "AKIA", "secret_access_key": "s3cret", "password": "" });
        seal_config(&mut cfg).unwrap();
        assert!(is_encrypted(cfg["secret_access_key"].as_str().unwrap()));
        assert_eq!(cfg["region"], "us-east-1");
        assert_eq!(cfg["password"], "", "empty secrets stay empty");
        let sealed_once = cfg.clone();
        seal_config(&mut cfg).unwrap();
        assert_eq!(cfg, sealed_once, "sealing is idempotent");
        assert_eq!(
            open_config(&cfg.to_string()).unwrap()["secret_access_key"],
            "s3cret"
        );
    }
}
