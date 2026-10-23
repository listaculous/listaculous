//! Passkey (WebAuthn) sign-in. Passkeys are registered as discoverable credentials, so signing in
//! needs no email: the browser or password manager (Bitwarden, iCloud Keychain, 1Password…) offers
//! the saved passkey and returns the user handle, which identifies the account.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use serde_json::{json, Value};
use webauthn_rs::prelude::*;

use crate::auth::{start_session, AuthUser};
use crate::db::{now, random_token};
use crate::error::{AppError, AppResult};
use crate::AppState;

const CHALLENGE_TTL_SECS: i64 = 300;

/// Builds the relying party from BASE_URL: the RP ID is its host, the allowed origin is BASE_URL
/// itself. Passkeys are bound to that host, so changing BASE_URL's domain invalidates them.
pub fn build(base_url: &str) -> anyhow::Result<Webauthn> {
    let origin = Url::parse(base_url)?;
    let rp_id = origin.host_str().ok_or_else(|| anyhow::anyhow!("BASE_URL has no host"))?.to_string();
    Ok(WebauthnBuilder::new(&rp_id, &origin)?.rp_name("Listaculous").build()?)
}

fn wa_err(e: WebauthnError) -> AppError {
    AppError::bad_request(format!("passkey verification failed: {e}"))
}

async fn save_challenge(st: &AppState, kind: &str, user_id: Option<i64>, state: &impl serde::Serialize) -> AppResult<String> {
    let id = random_token(24);
    let expires = (chrono::Utc::now() + chrono::Duration::seconds(CHALLENGE_TTL_SECS))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    sqlx::query("DELETE FROM webauthn_challenges WHERE expires_at < $1").bind(now()).execute(&st.db).await?;
    sqlx::query("INSERT INTO webauthn_challenges (id, kind, user_id, state, expires_at) VALUES ($1, $2, $3, $4, $5)")
        .bind(&id)
        .bind(kind)
        .bind(user_id)
        .bind(serde_json::to_string(state).map_err(|e| anyhow::anyhow!(e))?)
        .bind(expires)
        .execute(&st.db)
        .await?;
    Ok(id)
}

/// Consumes a challenge (single use): it's deleted in the same statement that reads it.
async fn take_challenge<T: serde::de::DeserializeOwned>(
    st: &AppState,
    id: &str,
    kind: &str,
    user_id: Option<i64>,
) -> AppResult<T> {
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "DELETE FROM webauthn_challenges WHERE id = $1 AND kind = $2 AND expires_at > $3 RETURNING state, user_id",
    )
    .bind(id)
    .bind(kind)
    .bind(now())
    .fetch_optional(&st.db)
    .await?;
    let (state, owner) = row.ok_or_else(|| AppError::bad_request("passkey request expired; please try again"))?;
    if owner != user_id {
        return Err(AppError::bad_request("passkey request expired; please try again"));
    }
    serde_json::from_str(&state).map_err(|e| anyhow::anyhow!("bad challenge state: {e}").into())
}

/// Returns the user's stable WebAuthn handle, creating it on first use.
async fn user_handle(st: &AppState, user_id: i64) -> AppResult<Uuid> {
    let (existing,): (Option<String>,) =
        sqlx::query_as("SELECT webauthn_id FROM users WHERE id = $1").bind(user_id).fetch_one(&st.db).await?;
    if let Some(u) = existing.and_then(|s| Uuid::parse_str(&s).ok()) {
        return Ok(u);
    }
    let u = Uuid::new_v4();
    sqlx::query("UPDATE users SET webauthn_id = $1 WHERE id = $2 AND webauthn_id IS NULL")
        .bind(u.to_string())
        .bind(user_id)
        .execute(&st.db)
        .await?;
    // Re-read in case a concurrent request set it first.
    let (id,): (String,) = sqlx::query_as("SELECT webauthn_id FROM users WHERE id = $1").bind(user_id).fetch_one(&st.db).await?;
    Uuid::parse_str(&id).map_err(|e| anyhow::anyhow!(e).into())
}

fn cred_key(id: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id)
}

// ---------------------------------------------------------------------------------------
// Registration (signed in)

pub async fn register_start(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let handle = user_handle(&st, user.id).await?;
    let existing: Vec<(String,)> =
        sqlx::query_as("SELECT passkey FROM passkeys WHERE user_id = $1").bind(user.id).fetch_all(&st.db).await?;
    // Exclude already-registered credentials so the same authenticator isn't added twice.
    let exclude: Vec<CredentialID> = existing
        .iter()
        .filter_map(|(p,)| serde_json::from_str::<Passkey>(p).ok())
        .map(|p| p.cred_id().clone())
        .collect();
    let (mut ccr, reg) = st
        .webauthn
        .start_passkey_registration(handle, &user.email, &user.email, Some(exclude))
        .map_err(wa_err)?;
    // Require a discoverable credential so sign-in works without typing an email.
    if let Some(sel) = ccr.public_key.authenticator_selection.as_mut() {
        sel.require_resident_key = true;
        sel.resident_key = Some(webauthn_rs_proto::ResidentKeyRequirement::Required);
    }
    let challenge_id = save_challenge(&st, "register", Some(user.id), &reg).await?;
    Ok(Json(json!({ "challenge_id": challenge_id, "options": ccr })))
}

#[derive(Deserialize)]
pub struct RegisterFinish {
    pub challenge_id: String,
    #[serde(default)]
    pub name: String,
    pub credential: RegisterPublicKeyCredential,
}

pub async fn register_finish(State(st): State<AppState>, user: AuthUser, Json(r): Json<RegisterFinish>) -> AppResult<Json<Value>> {
    let reg: PasskeyRegistration = take_challenge(&st, &r.challenge_id, "register", Some(user.id)).await?;
    let passkey = st.webauthn.finish_passkey_registration(&r.credential, &reg).map_err(wa_err)?;
    let name: String = r.name.trim().chars().take(60).collect();
    let name = if name.is_empty() { "Passkey".to_string() } else { name };
    sqlx::query(
        "INSERT INTO passkeys (user_id, name, credential_id, passkey, created_at) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (credential_id) DO NOTHING",
    )
    .bind(user.id)
    .bind(&name)
    .bind(cred_key(passkey.cred_id()))
    .bind(serde_json::to_string(&passkey).map_err(|e| anyhow::anyhow!(e))?)
    .bind(now())
    .execute(&st.db)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------------------
// Sign-in (public)

pub async fn login_start(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let ip = crate::public::client_ip(&st, &headers, addr);
    if !st.limiter.check(&format!("pk:{ip}")) {
        return Err(AppError::new(StatusCode::TOO_MANY_REQUESTS, "too many attempts; try again later"));
    }
    let (rcr, auth) = st.webauthn.start_discoverable_authentication().map_err(wa_err)?;
    let challenge_id = save_challenge(&st, "login", None, &auth).await?;
    Ok(Json(json!({ "challenge_id": challenge_id, "options": rcr })))
}

#[derive(Deserialize)]
pub struct LoginFinish {
    pub challenge_id: String,
    pub credential: PublicKeyCredential,
}

pub async fn login_finish(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(l): Json<LoginFinish>,
) -> AppResult<(CookieJar, Json<AuthUser>)> {
    let ip = crate::public::client_ip(&st, &headers, addr);
    if !st.auth_limiter.check(&format!("ip:{ip}")) {
        return Err(AppError::new(StatusCode::TOO_MANY_REQUESTS, "too many login attempts; try again later"));
    }
    let auth: DiscoverableAuthentication = take_challenge(&st, &l.challenge_id, "login", None).await?;
    let bad = || AppError::new(StatusCode::UNAUTHORIZED, "that passkey isn't registered here");
    let (handle, cred_id) = st.webauthn.identify_discoverable_authentication(&l.credential).map_err(|_| bad())?;

    let row: Option<(i64, i64, String, String, i64, String)> = sqlx::query_as(
        "SELECT u.id, u.account_id, u.email, u.password_hash, p.id, p.passkey FROM users u \
         JOIN passkeys p ON p.user_id = u.id WHERE u.webauthn_id = $1 AND p.credential_id = $2",
    )
    .bind(handle.to_string())
    .bind(cred_key(cred_id))
    .fetch_optional(&st.db)
    .await?;
    let (user_id, account_id, email, hash, passkey_id, stored) = row.ok_or_else(bad)?;
    let mut passkey: Passkey = serde_json::from_str(&stored).map_err(|e| anyhow::anyhow!(e))?;

    let result = st
        .webauthn
        .finish_discoverable_authentication(&l.credential, auth, &[DiscoverableKey::from(&passkey)])
        .map_err(wa_err)?;
    // Persist the updated signature counter / backup state.
    passkey.update_credential(&result);
    sqlx::query("UPDATE passkeys SET passkey = $1, last_used_at = $2 WHERE id = $3")
        .bind(serde_json::to_string(&passkey).map_err(|e| anyhow::anyhow!(e))?)
        .bind(now())
        .bind(passkey_id)
        .execute(&st.db)
        .await?;

    let jar = start_session(&st, jar, user_id, &hash, None).await?;
    Ok((jar, Json(AuthUser { id: user_id, account_id, email })))
}

// ---------------------------------------------------------------------------------------
// Management (signed in)

pub async fn list(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, name, created_at, last_used_at FROM passkeys WHERE user_id = $1 ORDER BY id",
    )
    .bind(user.id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(Value::Array(
        rows.into_iter()
            .map(|(id, name, created_at, last_used_at)| json!({ "id": id, "name": name, "created_at": created_at, "last_used_at": last_used_at }))
            .collect(),
    )))
}

#[derive(Deserialize)]
pub struct Rename {
    pub name: String,
}

pub async fn rename(State(st): State<AppState>, user: AuthUser, Path(id): Path<i64>, Json(r): Json<Rename>) -> AppResult<Json<Value>> {
    let name: String = r.name.trim().chars().take(60).collect();
    if name.is_empty() {
        return Err(AppError::bad_request("name is required"));
    }
    sqlx::query("UPDATE passkeys SET name = $1 WHERE id = $2 AND user_id = $3")
        .bind(name)
        .bind(id)
        .bind(user.id)
        .execute(&st.db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn remove(State(st): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<Value>> {
    let r = sqlx::query("DELETE FROM passkeys WHERE id = $1 AND user_id = $2").bind(id).bind(user.id).execute(&st.db).await?;
    if r.rows_affected() == 0 {
        return Err(AppError::not_found());
    }
    Ok(Json(json!({ "ok": true })))
}
