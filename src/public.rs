//! Unauthenticated endpoints: signup, double opt-in confirmation, unsubscribe, provider webhooks.

use std::net::SocketAddr;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Json;
use serde::Deserialize;

use crate::db::{now, random_token};
use crate::error::{AppError, AppResult};
use crate::mailer::{self, html_escape, OutgoingEmail};
use crate::models::{List, Site, LIST_COLS, SITE_COLS};
use crate::AppState;

#[derive(Deserialize, Default)]
pub struct SubscribeForm {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    /// Honeypot: real users never fill this hidden field.
    #[serde(default)]
    pub website: String,
}

pub fn client_ip(st: &AppState, headers: &HeaderMap, addr: SocketAddr) -> String {
    if st.cfg.trust_proxy {
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return ip;
        }
    }
    addr.ip().to_string()
}

pub fn valid_email(email: &str) -> bool {
    email.len() <= 254
        && email
            .parse::<lettre::Address>()
            .is_ok_and(|a| a.domain().contains('.') && !a.domain().ends_with('.'))
}

pub async fn subscribe(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(public_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let form: SubscribeForm = if is_json {
        serde_json::from_slice(&body).unwrap_or_default()
    } else {
        serde_urlencoded::from_bytes(&body).unwrap_or_default()
    };
    let ip = client_ip(&st, &headers, addr);
    let result = do_subscribe(&st, &public_id, form, &ip).await;

    match (result, is_json) {
        (Ok(list), true) => Json(serde_json::json!({
            "ok": true,
            "message": if list.double_opt_in != 0 { "Please check your inbox to confirm your subscription." } else { "You're subscribed!" }
        }))
        .into_response(),
        (Ok(list), false) => {
            if !list.redirect_url.is_empty() {
                return Redirect::to(&list.redirect_url).into_response();
            }
            let msg = if list.double_opt_in != 0 {
                "Almost done! Please check your inbox and click the link to confirm your subscription."
            } else {
                "You're subscribed. Thanks!"
            };
            page(StatusCode::OK, "Thanks!", msg)
        }
        (Err(e), true) => e.into_response(),
        (Err(e), false) => {
            let msg = if e.status.is_server_error() { "Something went wrong. Please try again later." } else { &e.message };
            page(e.status, "Oops", msg)
        }
    }
}

async fn do_subscribe(
    st: &AppState,
    public_id: &str,
    form: SubscribeForm,
    ip: &str,
) -> AppResult<List> {
    if !st.limiter.check(&format!("sub:{ip}")) {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests. Please try again later.",
        ));
    }
    let list: List = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LIST_COLS} FROM lists WHERE public_id = $1"
    )))
    .bind(public_id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(AppError::not_found)?;
    if !form.website.is_empty() {
        tracing::info!("honeypot triggered from {ip}");
        return Ok(list);
    }
    let email = form.email.trim().to_lowercase();
    if !valid_email(&email) {
        return Err(AppError::bad_request("Please enter a valid email address."));
    }
    let name: String = form.name.trim().chars().take(100).collect();

    let site: Site = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SITE_COLS} FROM sites WHERE id = $1"
    )))
    .bind(list.site_id)
    .fetch_one(&st.db)
    .await?;

    let outcome = upsert_subscription(
        st,
        &list,
        &email,
        &name,
        ip,
        "form",
        list.double_opt_in == 0,
    )
    .await?;
    if let SubscribeOutcome::NeedsConfirmation(token) = outcome {
        // At most one confirmation email per subscription per cooldown window, so the form can't
        // be used to flood someone's inbox (the claim is atomic across concurrent requests).
        let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(CONFIRM_COOLDOWN_MINUTES))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let claimed = sqlx::query(
            "UPDATE subscriptions SET confirm_sent_at = $1 WHERE token = $2 AND status = 'pending' \
             AND (confirm_sent_at IS NULL OR confirm_sent_at < $3)",
        )
        .bind(now())
        .bind(&token)
        .bind(cutoff)
        .execute(&st.db)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            return Ok(list);
        }
        let st = st.clone();
        let (site, list) = (site.clone(), list.clone());
        tokio::spawn(async move {
            // Bound concurrent confirmation sends so bursts can't overwhelm the provider.
            let _permit = CONFIRM_PERMITS.acquire().await;
            // Re-check after waiting: they may have confirmed, unsubscribed or bounced meanwhile.
            let still_pending: Result<Option<(i64,)>, _> = sqlx::query_as(
                "SELECT s.id FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id \
                 WHERE s.token = $1 AND s.status = 'pending' AND sb.status = 'active'",
            )
            .bind(&token)
            .fetch_optional(&st.db)
            .await;
            if !matches!(still_pending, Ok(Some(_))) {
                return;
            }
            if let Err(e) = send_confirmation(&st, &site, &list, &email, &name, &token).await {
                tracing::error!("failed to send confirmation to {email}: {e:#}");
            }
        });
    }
    Ok(list)
}

const CONFIRM_COOLDOWN_MINUTES: i64 = 10;
static CONFIRM_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub enum SubscribeOutcome {
    AlreadyConfirmed,
    Suppressed,
    Confirmed,
    NeedsConfirmation(String),
}

/// Creates or reactivates a subscription. Suppressed (bounced/complained) addresses are never
/// re-added. Responses to the visitor are identical in every case so list membership can't be probed.
pub async fn upsert_subscription(
    st: &AppState,
    list: &List,
    email: &str,
    name: &str,
    ip: &str,
    source: &str,
    confirm_now: bool,
) -> AppResult<SubscribeOutcome> {
    // The transition is decided from a read and applied with a compare-and-set UPDATE; if a
    // concurrent confirm/unsubscribe changed the row in between, re-decide from fresh state.
    for _ in 0..3 {
        if let Some(outcome) =
            try_upsert_subscription(st, list, email, name, ip, source, confirm_now).await?
        {
            return Ok(outcome);
        }
    }
    Err(AppError::new(StatusCode::CONFLICT, "Please try again."))
}

async fn try_upsert_subscription(
    st: &AppState,
    list: &List,
    email: &str,
    name: &str,
    ip: &str,
    source: &str,
    confirm_now: bool,
) -> AppResult<Option<SubscribeOutcome>> {
    let ts = now();
    let mut tx = st.db.begin().await?;
    sqlx::query(
        "INSERT INTO subscribers (site_id, email, name, status, created_at) VALUES ($1, $2, $3, 'active', $4) \
         ON CONFLICT (site_id, email) DO NOTHING",
    )
    .bind(list.site_id)
    .bind(email)
    .bind(name)
    .bind(&ts)
    .execute(&mut *tx)
    .await?;
    let (subscriber_id, status, existing_name): (i64, String, String) = sqlx::query_as(
        "SELECT id, status, name FROM subscribers WHERE site_id = $1 AND email = $2",
    )
    .bind(list.site_id)
    .bind(email)
    .fetch_one(&mut *tx)
    .await?;
    if status != "active" {
        tx.commit().await?;
        return Ok(Some(SubscribeOutcome::Suppressed));
    }
    if existing_name.is_empty() && !name.is_empty() {
        sqlx::query("UPDATE subscribers SET name = $1 WHERE id = $2")
            .bind(name)
            .bind(subscriber_id)
            .execute(&mut *tx)
            .await?;
    }

    let existing: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT id, status, token FROM subscriptions WHERE list_id = $1 AND subscriber_id = $2",
    )
    .bind(list.id)
    .bind(subscriber_id)
    .fetch_optional(&mut *tx)
    .await?;
    let new_status = if confirm_now { "confirmed" } else { "pending" };
    let confirmed_at = if confirm_now { Some(ts.clone()) } else { None };
    let outcome = match existing {
        Some((_, s, _)) if s == "confirmed" => SubscribeOutcome::AlreadyConfirmed,
        Some((id, old_status, old_token)) => {
            // A new opt-in cycle after unsubscribing gets a fresh token so old confirmation
            // emails can't confirm it. A still-pending signup keeps its token (resend).
            let token = if old_status == "unsubscribed" {
                random_token(24)
            } else {
                old_token.clone()
            };
            let r = sqlx::query(
                "UPDATE subscriptions SET status = $1, consent_ip = $2, consent_source = $3, created_at = $4, \
                 confirmed_at = $5, unsubscribed_at = NULL, \
                 confirm_sent_at = CASE WHEN token = $6 THEN confirm_sent_at ELSE NULL END, token = $6 WHERE id = $7 AND status = $8 AND token = $9",
            )
            .bind(new_status)
            .bind(ip)
            .bind(source)
            .bind(&ts)
            .bind(confirmed_at)
            .bind(&token)
            .bind(id)
            .bind(&old_status)
            .bind(&old_token)
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() == 0 {
                return Ok(None);
            }
            if confirm_now {
                SubscribeOutcome::Confirmed
            } else {
                SubscribeOutcome::NeedsConfirmation(token)
            }
        }
        None => {
            let token = random_token(24);
            let r = sqlx::query(
                "INSERT INTO subscriptions (list_id, subscriber_id, status, token, consent_ip, consent_source, created_at, confirmed_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (list_id, subscriber_id) DO NOTHING",
            )
            .bind(list.id)
            .bind(subscriber_id)
            .bind(new_status)
            .bind(&token)
            .bind(ip)
            .bind(source)
            .bind(&ts)
            .bind(confirmed_at)
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() == 0 {
                // A concurrent signup created it first; re-decide from fresh state.
                return Ok(None);
            }
            if confirm_now {
                SubscribeOutcome::Confirmed
            } else {
                SubscribeOutcome::NeedsConfirmation(token)
            }
        }
    };
    tx.commit().await?;
    Ok(Some(outcome))
}

async fn send_confirmation(
    st: &AppState,
    site: &Site,
    list: &List,
    email: &str,
    name: &str,
    token: &str,
) -> anyhow::Result<()> {
    let url = format!("{}/confirm/{token}", st.cfg.base_url);
    let html = format!(
        r#"<div style="font-family:-apple-system,Segoe UI,Helvetica,Arial,sans-serif;max-width:560px;margin:0 auto;padding:24px;font-size:16px;color:#222">
<p>Please confirm your subscription to <strong>{list}</strong> from {site}.</p>
<p><a href="{url}" style="display:inline-block;background:#4f46e5;color:#fff;padding:12px 22px;border-radius:8px;font-weight:600;text-decoration:none">Confirm subscription</a></p>
<p style="font-size:13px;color:#777">If you didn't request this, just ignore this email and you won't be subscribed.</p>
<p style="font-size:12px;color:#999">{address}</p></div>"#,
        list = html_escape(&list.name),
        site = html_escape(&site.name),
        address = html_escape(&site.physical_address).replace('\n', "<br>"),
    );
    let text = format!(
        "Please confirm your subscription to {} from {}:\n\n{url}\n\nIf you didn't request this, ignore this email.\n\n{}",
        list.name, site.name, site.physical_address
    );
    let msg = mailer::build_message(
        site,
        &OutgoingEmail {
            to_email: email.into(),
            to_name: name.into(),
            subject: format!("Confirm your subscription to {}", list.name),
            html,
            text,
            unsubscribe_url: None,
            list_id: None,
        },
    )?;
    mailer::provider_for(site, &st.http)?
        .send(&msg)
        .await
        .map_err(anyhow::Error::new)?;
    Ok(())
}

pub async fn confirm(State(st): State<AppState>, Path(token): Path<String>) -> AppResult<Response> {
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT s.id, s.status, l.name FROM subscriptions s JOIN lists l ON l.id = s.list_id WHERE s.token = $1",
    )
    .bind(&token)
    .fetch_optional(&st.db)
    .await?;
    let Some((id, status, list_name)) = row else {
        return Ok(page(
            StatusCode::NOT_FOUND,
            "Link not valid",
            "This confirmation link is invalid or has expired.",
        ));
    };
    if status == "pending" {
        sqlx::query(
            "UPDATE subscriptions SET status = 'confirmed', confirmed_at = $1 WHERE id = $2 AND token = $3 \
             AND status = 'pending' AND subscriber_id IN (SELECT id FROM subscribers WHERE status = 'active')",
        )
        .bind(now())
        .bind(id)
        .bind(&token)
            .execute(&st.db)
            .await?;
    } else if status == "unsubscribed" {
        return Ok(page(
            StatusCode::OK,
            "Unsubscribed",
            "You previously unsubscribed from this list. Sign up again to resubscribe.",
        ));
    }
    Ok(page(
        StatusCode::OK,
        "Subscription confirmed",
        &format!("You're subscribed to {list_name}. Thanks!"),
    ))
}

pub async fn unsubscribe_page(
    State(st): State<AppState>,
    Path(token): Path<String>,
) -> AppResult<Response> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT l.name, si.name, sb.email FROM subscriptions s JOIN lists l ON l.id = s.list_id \
         JOIN sites si ON si.id = l.site_id JOIN subscribers sb ON sb.id = s.subscriber_id WHERE s.token = $1",
    )
    .bind(&token)
    .fetch_optional(&st.db)
    .await?;
    let Some((list_name, site_name, email)) = row else {
        return Ok(page(
            StatusCode::NOT_FOUND,
            "Link not valid",
            "This unsubscribe link is invalid.",
        ));
    };
    // GET never unsubscribes on its own: link scanners prefetch URLs. The button posts back.
    let body = format!(
        r#"<p>Unsubscribe <strong>{email}</strong> from <strong>{list}</strong>?</p>
<form method="post"><input type="hidden" name="scope" value="list"><button type="submit">Unsubscribe from {list}</button></form>
<form method="post" style="margin-top:12px"><input type="hidden" name="scope" value="all"><button type="submit" class="secondary">Unsubscribe from everything from {site}</button></form>"#,
        email = html_escape(&email),
        list = html_escape(&list_name),
        site = html_escape(&site_name),
    );
    Ok(page_html(StatusCode::OK, "Unsubscribe", &body))
}

/// Handles both the form button and RFC 8058 one-click POSTs (body `List-Unsubscribe=One-Click`).
pub async fn unsubscribe(
    State(st): State<AppState>,
    Path(token): Path<String>,
    body: Bytes,
) -> AppResult<Response> {
    let form: Vec<(String, String)> = serde_urlencoded::from_bytes(&body).unwrap_or_default();
    let all = form.iter().any(|(k, v)| k == "scope" && v == "all");
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT l.name FROM subscriptions s JOIN lists l ON l.id = s.list_id WHERE s.token = $1",
    )
    .bind(&token)
    .fetch_optional(&st.db)
    .await?;
    let Some((list_name,)) = row else {
        return Ok(page(
            StatusCode::NOT_FOUND,
            "Link not valid",
            "This unsubscribe link is invalid.",
        ));
    };
    // Each update is a single statement keyed by the token itself, so a token that was rotated by a
    // later re-subscription can never affect the newer subscription cycle.
    // The token-bearing row is updated first, keyed by the token. That row write locks it (and is
    // re-checked against the token on Postgres), so a concurrent re-subscription that rotates the
    // token can't be overridden. For "all", the other lists are updated in the same transaction.
    let ts = now();
    let mut tx = st.db.begin().await?;
    // Lock the subscriber row first so concurrent unsubscribes for the same person (via different
    // newsletters' tokens) serialize instead of deadlocking on each other's subscription rows.
    sqlx::query(
        "UPDATE subscribers SET status = status WHERE id IN (SELECT subscriber_id FROM subscriptions WHERE token = $1)",
    )
    .bind(&token)
    .execute(&mut *tx)
    .await?;
    let own: Option<(i64,)> = sqlx::query_as(
        "UPDATE subscriptions SET status = 'unsubscribed', unsubscribed_at = COALESCE(unsubscribed_at, $1) \
         WHERE token = $2 RETURNING subscriber_id",
    )
    .bind(&ts)
    .bind(&token)
    .fetch_optional(&mut *tx)
    .await?;
    if let (true, Some((subscriber_id,))) = (all, own) {
        sqlx::query(
            "UPDATE subscriptions SET status = 'unsubscribed', unsubscribed_at = $1 WHERE subscriber_id = $2 AND status <> 'unsubscribed'",
        )
        .bind(&ts)
        .bind(subscriber_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let msg = if all {
        "You've been unsubscribed from all emails.".to_string()
    } else {
        format!("You've been unsubscribed from {list_name}.")
    };
    Ok(page(StatusCode::OK, "Unsubscribed", &msg))
}

// ---------------------------------------------------------------------------------------
// Amazon SES → SNS bounce/complaint webhook.
// Configure an SNS topic (HTTPS subscription) pointing at /webhooks/ses/{site_id}/{webhook_secret}.

pub async fn ses_webhook(
    State(st): State<AppState>,
    Path((site_id, secret)): Path<(i64, String)>,
    body: Bytes,
) -> AppResult<StatusCode> {
    let row: Option<(String,)> = sqlx::query_as("SELECT webhook_secret FROM sites WHERE id = $1")
        .bind(site_id)
        .fetch_optional(&st.db)
        .await?;
    let Some((expected,)) = row else {
        return Err(AppError::not_found());
    };
    if !constant_time_eq(expected.as_bytes(), secret.as_bytes()) {
        return Err(AppError::not_found());
    }
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| AppError::bad_request("invalid json"))?;
    match v["Type"].as_str() {
        Some("SubscriptionConfirmation") => {
            let url = v["SubscribeURL"].as_str().unwrap_or_default();
            let parsed =
                url::Url::parse(url).map_err(|_| AppError::bad_request("bad SubscribeURL"))?;
            let host = parsed.host_str().unwrap_or_default();
            if parsed.scheme() != "https"
                || !(host.starts_with("sns.") && host.ends_with(".amazonaws.com"))
            {
                return Err(AppError::bad_request("SubscribeURL is not an SNS endpoint"));
            }
            st.http
                .get(url)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!(e))?
                .error_for_status()
                .map_err(|e| anyhow::anyhow!(e))?;
            tracing::info!("confirmed SNS subscription for site {site_id}");
        }
        Some("Notification") => {
            let msg: serde_json::Value =
                serde_json::from_str(v["Message"].as_str().unwrap_or("{}")).unwrap_or_default();
            handle_ses_event(&st, site_id, &msg).await?;
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}

async fn handle_ses_event(st: &AppState, site_id: i64, msg: &serde_json::Value) -> AppResult<()> {
    let kind = msg["notificationType"]
        .as_str()
        .or_else(|| msg["eventType"].as_str())
        .unwrap_or_default();
    let (status, recipients, detail) = match kind {
        "Bounce" => {
            let b = &msg["bounce"];
            if b["bounceType"].as_str() != Some("Permanent") {
                // Transient bounces (mailbox full, etc.) are logged but don't suppress.
                (
                    "",
                    &b["bouncedRecipients"],
                    format!("transient: {}", b["bounceSubType"].as_str().unwrap_or("")),
                )
            } else {
                (
                    "bounced",
                    &b["bouncedRecipients"],
                    b["bounceSubType"].as_str().unwrap_or("").to_string(),
                )
            }
        }
        "Complaint" => {
            let c = &msg["complaint"];
            (
                "complained",
                &c["complainedRecipients"],
                c["complaintFeedbackType"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            )
        }
        _ => return Ok(()),
    };
    let ts = now();
    for r in recipients.as_array().into_iter().flatten() {
        let Some(email) = r["emailAddress"].as_str() else {
            continue;
        };
        let email = extract_address(email).to_lowercase();
        sqlx::query("INSERT INTO email_events (site_id, email, kind, detail, created_at) VALUES ($1, $2, $3, $4, $5)")
            .bind(site_id)
            .bind(&email)
            .bind(kind.to_lowercase())
            .bind(&detail)
            .bind(&ts)
            .execute(&st.db)
            .await?;
        if status.is_empty() {
            continue;
        }
        suppress(st, site_id, &email, status).await?;
        tracing::info!("suppressed {email} on site {site_id}: {status}");
    }
    Ok(())
}

/// Marks a subscriber as suppressed and unsubscribes them from every list on the site.
pub async fn suppress(st: &AppState, site_id: i64, email: &str, status: &str) -> AppResult<()> {
    let ts = now();
    // Upsert so a bounce for an address we haven't stored yet (e.g. a test send) still blocks it later.
    sqlx::query(
        "INSERT INTO subscribers (site_id, email, name, status, created_at) VALUES ($1, $2, '', $3, $4) \
         ON CONFLICT (site_id, email) DO UPDATE SET status = excluded.status",
    )
    .bind(site_id)
    .bind(email)
    .bind(status)
    .bind(&ts)
    .execute(&st.db)
    .await?;
    sqlx::query(
        "UPDATE subscriptions SET status = 'unsubscribed', unsubscribed_at = $1 WHERE status <> 'unsubscribed' AND subscriber_id IN \
         (SELECT id FROM subscribers WHERE site_id = $2 AND email = $3)",
    )
    .bind(&ts)
    .bind(site_id)
    .bind(email)
    .execute(&st.db)
    .await?;
    Ok(())
}

fn extract_address(s: &str) -> &str {
    match (s.rfind('<'), s.rfind('>')) {
        (Some(a), Some(b)) if a < b => &s[a + 1..b],
        _ => s.trim(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------------------

fn page(status: StatusCode, title: &str, message: &str) -> Response {
    page_html(status, title, &format!("<p>{}</p>", html_escape(message)))
}

fn page_html(status: StatusCode, title: &str, body: &str) -> Response {
    // Subscriber-facing pages: neutral, so they suit any of the user's sites, with a small
    // "powered by" mark. The envelope icon is Lucide's "mail-check" (ISC license).
    let icon_color = if status.is_success() {
        "#4f46e5"
    } else {
        "#dc2626"
    };
    let html = format!(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex"><meta name="color-scheme" content="light dark"><title>{t}</title>
<link rel="icon" type="image/png" href="/brand/favicon.png"><style>
:root{{--bg:#f6f7fb;--card:#fff;--text:#111827;--muted:#6b7280;--border:#e5e7ef;--accent:#4f46e5;--accent-soft:#eef2ff}}
@media (prefers-color-scheme:dark){{:root{{--bg:#0b0d14;--card:#131622;--text:#eef0f7;--muted:#8a90a6;--border:#232838;--accent:#818cf8;--accent-soft:rgba(129,140,248,.14)}}}}
*{{box-sizing:border-box}}
body{{font:15px/1.6 Inter,-apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;background:var(--bg);color:var(--text);margin:0;min-height:100vh;display:flex;flex-direction:column;align-items:center;justify-content:center;padding:32px 16px}}
.card{{width:100%;max-width:460px;background:var(--card);border:1px solid var(--border);border-radius:16px;padding:36px 32px;box-shadow:0 12px 32px -12px rgba(30,27,75,.18);text-align:center;position:relative;overflow:hidden}}
.card:before{{content:"";position:absolute;top:0;left:0;right:0;height:4px;background:linear-gradient(90deg,#4f46e5,#7c3aed,#f59e0b)}}
.icon{{width:60px;height:60px;border-radius:50%;display:inline-grid;place-items:center;background:var(--accent-soft);margin-bottom:16px}}
h1{{font-size:22px;letter-spacing:-.02em;margin:0 0 10px}}p{{color:var(--muted);margin:0 0 18px}}p strong{{color:var(--text)}}
form{{margin:0 0 10px}}button{{width:100%;font:inherit;font-weight:600;border-radius:10px;padding:12px 16px;cursor:pointer;border:1px solid var(--accent);background:var(--accent);color:#fff}}
@media (prefers-color-scheme:dark){{button{{color:#0b0d14}}}}
button:hover{{filter:brightness(1.08)}}button.secondary{{background:transparent;color:var(--accent)}}
.foot{{margin-top:22px;font-size:12px;color:var(--muted);display:flex;align-items:center;gap:6px}}.foot img{{width:18px;height:18px}}
</style></head><body><div class="card">
<div class="icon"><svg xmlns="http://www.w3.org/2000/svg" width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="{icon_color}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 13V6a2 2 0 0 0-2-2H4a2 2 0 0 0-2 2v12c0 1.1.9 2 2 2h8"/><path d="m22 7-8.97 5.7a1.94 1.94 0 0 1-2.06 0L2 7"/><path d="m16 19 2 2 4-4"/></svg></div>
<h1>{t}</h1>{body}</div>
<div class="foot"><img src="/brand/favicon.png" alt="">Powered by Listaculous</div></body></html>"##,
        t = html_escape(title)
    );
    (status, Html(html)).into_response()
}
