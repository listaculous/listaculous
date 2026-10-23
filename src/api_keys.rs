//! API keys: long-lived bearer tokens for scripts and AI agents. Keys act as the user who created
//! them (same account scope) but can't manage credentials (see `AuthUser::require_session`).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthUser;
use crate::db::{now, random_token, sha256_hex};
use crate::error::{AppError, AppResult};
use crate::AppState;

pub const TOKEN_PREFIX: &str = "lc_";

pub async fn authenticate(st: &AppState, token: &str) -> AppResult<AuthUser> {
    if !token.starts_with(TOKEN_PREFIX) {
        return Err(AppError::unauthorized());
    }
    let hash = sha256_hex(token);
    let ts = now();
    let row: Option<(i64, i64, i64, String)> = sqlx::query_as(
        "SELECT k.id, u.id, u.account_id, u.email FROM api_keys k JOIN users u ON u.id = k.user_id \
         WHERE k.token_hash = $1 AND (k.expires_at IS NULL OR k.expires_at > $2)",
    )
    .bind(&hash)
    .bind(&ts)
    .fetch_optional(&st.db)
    .await?;
    let (key_id, id, account_id, email) = row.ok_or_else(AppError::unauthorized)?;
    // Record usage at most once a minute per key to keep writes low.
    let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    sqlx::query("UPDATE api_keys SET last_used_at = $1 WHERE id = $2 AND (last_used_at IS NULL OR last_used_at < $3)")
        .bind(&ts)
        .bind(key_id)
        .bind(cutoff)
        .execute(&st.db)
        .await?;
    let mut user = AuthUser::new(id, account_id, email);
    user.via_api_key = true;
    Ok(user)
}

pub async fn list(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    user.require_session()?;
    let rows: Vec<(i64, String, String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, name, prefix, created_at, last_used_at, expires_at FROM api_keys WHERE user_id = $1 ORDER BY id DESC",
    )
    .bind(user.id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(Value::Array(
        rows.into_iter()
            .map(|(id, name, prefix, created_at, last_used_at, expires_at)| {
                json!({ "id": id, "name": name, "prefix": prefix, "created_at": created_at,
                        "last_used_at": last_used_at, "expires_at": expires_at })
            })
            .collect(),
    )))
}

#[derive(Deserialize)]
pub struct CreateKey {
    pub name: String,
    /// Optional lifetime in days; omit for a key that never expires.
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

/// Creates a key and returns the plaintext token. It is shown exactly once.
pub async fn create(
    State(st): State<AppState>,
    user: AuthUser,
    Json(c): Json<CreateKey>,
) -> AppResult<(StatusCode, Json<Value>)> {
    user.require_session()?;
    let name: String = c.name.trim().chars().take(60).collect();
    if name.is_empty() {
        return Err(AppError::bad_request("name is required"));
    }
    let expires_at = match c.expires_in_days {
        Some(d) if !(1..=3650).contains(&d) => {
            return Err(AppError::bad_request(
                "expires_in_days must be between 1 and 3650",
            ))
        }
        Some(d) => Some(
            (chrono::Utc::now() + chrono::Duration::days(d))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
        None => None,
    };
    let token = format!("{TOKEN_PREFIX}{}", random_token(32));
    let prefix: String = token.chars().take(10).collect();
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO api_keys (user_id, name, prefix, token_hash, created_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(user.id)
    .bind(&name)
    .bind(&prefix)
    .bind(sha256_hex(&token))
    .bind(now())
    .bind(&expires_at)
    .fetch_one(&st.db)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({ "id": id, "name": name, "prefix": prefix, "expires_at": expires_at, "token": token }),
        ),
    ))
}

pub async fn revoke(
    State(st): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    user.require_session()?;
    let r = sqlx::query("DELETE FROM api_keys WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user.id)
        .execute(&st.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::not_found());
    }
    Ok(Json(json!({ "ok": true })))
}
