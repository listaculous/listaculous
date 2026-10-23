use anyhow::Context;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::any::AnyPoolOptions;
use sqlx::AnyPool;
use sqlx::Executor;

pub type Db = AnyPool;

pub async fn connect(url: &str) -> anyhow::Result<Db> {
    sqlx::any::install_default_drivers();
    let is_sqlite = url.starts_with("sqlite:");
    if is_sqlite {
        ensure_sqlite_dir(url);
    }
    let pool = AnyPoolOptions::new()
        .max_connections(if is_sqlite { 4 } else { 10 })
        .after_connect(move |conn, _| {
            Box::pin(async move {
                if is_sqlite {
                    conn.execute("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")
                        .await?;
                }
                Ok(())
            })
        })
        .connect(url)
        .await
        .with_context(|| format!("connecting to database {}", redact(url)))?;

    if is_sqlite {
        sqlx::migrate!("migrations/sqlite").run(&pool).await?;
    } else {
        sqlx::migrate!("migrations/postgres").run(&pool).await?;
    }
    Ok(pool)
}

fn ensure_sqlite_dir(url: &str) {
    let path = url
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");
    let path = path.split('?').next().unwrap_or("");
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
}

fn redact(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) if u.password().is_some() => {
            let _ = u.set_password(Some("***"));
            u.to_string()
        }
        _ => url.to_string(),
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}
