use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use std::net::SocketAddr;

use argon2::Argon2;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};

use crate::db::{now, random_token, sha256_hex};
use crate::error::{AppError, AppResult};
use crate::AppState;

const COOKIE: &str = "lc_session";
const SESSION_DAYS: i64 = 30;

#[derive(Clone, Serialize)]
pub struct AuthUser {
    pub id: i64,
    pub account_id: i64,
    pub email: String,
    /// True when authenticated with an API key rather than a browser session.
    #[serde(skip)]
    pub via_api_key: bool,
}

impl AuthUser {
    pub fn new(id: i64, account_id: i64, email: String) -> Self {
        Self {
            id,
            account_id,
            email,
            via_api_key: false,
        }
    }

    /// Credential management (password, passkeys, API keys) requires a real browser session, so a
    /// leaked API key can't be used to mint more keys or lock the owner out.
    pub fn require_session(&self) -> AppResult<()> {
        if self.via_api_key {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "this action requires signing in to the web app; API keys can't manage credentials",
            ));
        }
        Ok(())
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // API key: `Authorization: Bearer lc_…` (for scripts and AI agents).
        if let Some(key) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            })
        {
            return crate::api_keys::authenticate(state, key.trim()).await;
        }
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get(COOKIE)
            .map(|c| c.value().to_string())
            .ok_or_else(AppError::unauthorized)?;
        let row: Option<(i64, i64, String)> = sqlx::query_as(
            "SELECT u.id, u.account_id, u.email FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = $1 AND s.expires_at > $2",
        )
        .bind(sha256_hex(&token))
        .bind(now())
        .fetch_optional(&state.db)
        .await?;
        let (id, account_id, email) = row.ok_or_else(AppError::unauthorized)?;
        Ok(AuthUser::new(id, account_id, email))
    }
}

#[derive(Deserialize)]
pub struct Credentials {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub account_name: String,
}

pub async fn status(State(st): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&st.db)
        .await?;
    Ok(Json(serde_json::json!({ "needs_setup": n == 0 })))
}

/// First-run setup: creates the first account + user. Only allowed while no users exist.
pub async fn setup(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(c): Json<Credentials>,
) -> AppResult<(CookieJar, Json<AuthUser>)> {
    let ip = crate::public::client_ip(&st, &headers, addr);
    if !st.auth_limiter.check(&format!("ip:{ip}")) {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts; try again later",
        ));
    }
    // Cheap check first so a completed install never does Argon2 work for setup requests.
    let (existing,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&st.db)
        .await?;
    if existing > 0 {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "setup already completed",
        ));
    }
    let email = c.email.trim().to_lowercase();
    if !email.contains('@') || c.password.len() < 10 {
        return Err(AppError::bad_request(
            "valid email and a password of at least 10 characters required",
        ));
    }
    let hash = hash_password(c.password.clone()).await?;
    let mut tx = st.db.begin().await?;
    // The primary key makes this fail for a concurrent second setup on any database.
    let locked = sqlx::query("INSERT INTO setup_lock (id) VALUES (1) ON CONFLICT (id) DO NOTHING")
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&mut *tx)
        .await?;
    if !locked || n > 0 {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "setup already completed",
        ));
    }
    let name = if c.account_name.trim().is_empty() {
        "My Account".to_string()
    } else {
        c.account_name.trim().to_string()
    };
    let (account_id,): (i64,) =
        sqlx::query_as("INSERT INTO accounts (name, created_at) VALUES ($1, $2) RETURNING id")
            .bind(name)
            .bind(now())
            .fetch_one(&mut *tx)
            .await?;
    let (user_id,): (i64,) = sqlx::query_as(
        "INSERT INTO users (account_id, email, password_hash, created_at) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(account_id)
    .bind(&email)
    .bind(&hash)
    .bind(now())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let jar = start_session(&st, jar, user_id, &hash, None).await?;
    Ok((jar, Json(AuthUser::new(user_id, account_id, email))))
}

pub async fn login(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(c): Json<Credentials>,
) -> AppResult<(CookieJar, Json<AuthUser>)> {
    let email = c.email.trim().to_lowercase();
    let ip = crate::public::client_ip(&st, &headers, addr);
    if !st.auth_limiter.check(&format!("ip:{ip}"))
        || !st.auth_limiter.check(&format!("email:{email}"))
    {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many login attempts; try again later",
        ));
    }
    let row: Option<(i64, i64, String)> =
        sqlx::query_as("SELECT id, account_id, password_hash FROM users WHERE email = $1")
            .bind(&email)
            .fetch_optional(&st.db)
            .await?;
    let bad = || AppError::new(StatusCode::UNAUTHORIZED, "invalid email or password");
    let (id, account_id, hash) = match row {
        Some(r) => r,
        None => {
            // Equalize timing with the found-user path.
            let _ = hash_password(c.password).await;
            return Err(bad());
        }
    };
    if !verify_password(c.password, hash.clone()).await? {
        return Err(bad());
    }
    let jar = start_session(&st, jar, id, &hash, None)
        .await
        .map_err(|e| {
            if e.status == StatusCode::CONFLICT {
                bad()
            } else {
                e
            }
        })?;
    Ok((jar, Json(AuthUser::new(id, account_id, email))))
}

pub async fn logout(State(st): State<AppState>, jar: CookieJar) -> AppResult<CookieJar> {
    if let Some(c) = jar.get(COOKIE) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(sha256_hex(c.value()))
            .execute(&st.db)
            .await?;
    }
    Ok(jar.remove(Cookie::build(COOKIE).path("/")))
}

pub async fn me(user: AuthUser) -> Json<AuthUser> {
    Json(user)
}

#[derive(Deserialize)]
pub struct PasswordChange {
    pub current_password: String,
    pub new_password: String,
}

pub async fn change_password(
    State(st): State<AppState>,
    user: AuthUser,
    jar: CookieJar,
    Json(p): Json<PasswordChange>,
) -> AppResult<(CookieJar, Json<serde_json::Value>)> {
    user.require_session()?;
    if !st.auth_limiter.check(&format!("user:{}", user.id)) {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts; try again later",
        ));
    }
    if p.new_password.len() < 10 {
        return Err(AppError::bad_request(
            "password must be at least 10 characters",
        ));
    }
    let (hash,): (String,) = sqlx::query_as("SELECT password_hash FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(&st.db)
        .await?;
    if !verify_password(p.current_password, hash.clone()).await? {
        return Err(AppError::bad_request("current password is incorrect"));
    }
    let new_hash = hash_password(p.new_password).await?;
    // Revokes every existing session (e.g. a stolen cookie) and starts a fresh one for this browser.
    let jar = start_session(&st, jar, user.id, &hash, Some(&new_hash))
        .await
        .map_err(|e| {
            if e.status == StatusCode::CONFLICT {
                AppError::bad_request("password was changed concurrently; try again")
            } else {
                e
            }
        })?;
    Ok((jar, Json(serde_json::json!({ "ok": true }))))
}

/// Bounds concurrent Argon2 work (each run uses ~19 MB) so floods can't exhaust memory/CPU.
static HASH_PERMITS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)));

/// Argon2 is deliberately expensive, so it runs on the blocking pool instead of a Tokio worker.
async fn hash_password(pw: String) -> AppResult<String> {
    let permit = HASH_PERMITS
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // The permit moves into the blocking task so it's held until hashing really finishes, even if
    // the request future is cancelled.
    let hashed = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(pw.as_bytes(), &salt)
            .map(|h| h.to_string())
    })
    .await
    .map_err(|e| anyhow::anyhow!("hash task: {e}"))?;
    Ok(hashed.map_err(|e| anyhow::anyhow!("hash error: {e}"))?)
}

async fn verify_password(pw: String, hash: String) -> AppResult<bool> {
    let permit = HASH_PERMITS
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let ok = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        PasswordHash::new(&hash).map(|parsed| {
            Argon2::default()
                .verify_password(pw.as_bytes(), &parsed)
                .is_ok()
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("verify task: {e}"))?;
    Ok(ok.map_err(|e| anyhow::anyhow!("bad hash: {e}"))?)
}

/// Creates a session for a user whose password hash was verified to be `verified_hash`, optionally
/// replacing the password (`new_hash`) and revoking all other sessions.
///
/// All of it happens in one transaction that starts with a conditional UPDATE of the user row. That
/// row write serializes concurrent logins/password changes on every database (Postgres row lock with
/// WHERE re-check; SQLite write lock), so a session can't be created against a stale password and a
/// stale password change can't overwrite a newer one.
pub(crate) async fn start_session(
    st: &AppState,
    jar: CookieJar,
    user_id: i64,
    verified_hash: &str,
    new_hash: Option<&str>,
) -> AppResult<CookieJar> {
    let token = random_token(32);
    let expires = (chrono::Utc::now() + chrono::Duration::days(SESSION_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut tx = st.db.begin().await?;
    let r = sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2 AND password_hash = $3")
        .bind(new_hash.unwrap_or(verified_hash))
        .bind(user_id)
        .bind(verified_hash)
        .execute(&mut *tx)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "password changed; sign in again",
        ));
    }
    if new_hash.is_some() {
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM sessions WHERE expires_at < $1")
        .bind(now())
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(sha256_hex(&token))
        .bind(user_id)
        .bind(expires)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let cookie = Cookie::build((COOKIE, token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .secure(st.cfg.base_url.starts_with("https://"))
        .max_age(time_days(SESSION_DAYS))
        .build();
    Ok(jar.add(cookie))
}

fn time_days(d: i64) -> time::Duration {
    time::Duration::days(d)
}
