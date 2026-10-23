//! Authenticated admin API. Every query is scoped to the caller's account.

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthUser;
use crate::db::{now, random_token};
use crate::error::{AppError, AppResult};
use crate::mailer;
use crate::models::*;
use crate::public::{suppress, upsert_subscription, valid_email};
use crate::AppState;

type J = AppResult<Json<Value>>;

async fn site_for(st: &AppState, u: &AuthUser, id: i64) -> AppResult<Site> {
    sqlx::query_as(&format!(
        "SELECT {SITE_COLS} FROM sites WHERE id = $1 AND account_id = $2"
    ))
    .bind(id)
    .bind(u.account_id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(AppError::not_found)
}

async fn list_for(st: &AppState, u: &AuthUser, id: i64) -> AppResult<(List, Site)> {
    let list: List = sqlx::query_as(&format!("SELECT {LIST_COLS} FROM lists WHERE id = $1"))
        .bind(id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(AppError::not_found)?;
    let site = site_for(st, u, list.site_id).await?;
    Ok((list, site))
}

async fn campaign_for(st: &AppState, u: &AuthUser, id: i64) -> AppResult<(Campaign, List, Site)> {
    let c: Campaign = sqlx::query_as(&format!(
        "SELECT {CAMPAIGN_COLS} FROM campaigns WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(AppError::not_found)?;
    let (list, site) = list_for(st, u, c.list_id).await?;
    Ok((c, list, site))
}

fn site_json(s: &Site, base_url: &str) -> Value {
    let mut v = serde_json::to_value(s).unwrap();
    v["provider_config"] = s.masked_provider_config();
    v["webhook_url"] = format!("{base_url}/webhooks/ses/{}/{}", s.id, s.webhook_secret).into();
    // Relative, cache-busted by the fetch time so a refreshed icon shows immediately.
    v["icon_url"] = match &s.icon_type {
        Some(_) => format!(
            "/api/sites/{}/icon?v={}",
            s.id,
            s.icon_checked_at.as_deref().unwrap_or("")
        )
        .into(),
        None => Value::Null,
    };
    v
}

// ---------------------------------------------------------------------------------------
// Dashboard

pub async fn dashboard(State(st): State<AppState>, u: AuthUser) -> J {
    let rows: Vec<(i64, String, i64, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT si.id, si.name, l.id, l.name, \
           COUNT(CASE WHEN s.status = 'confirmed' THEN 1 END), \
           COUNT(CASE WHEN s.status = 'pending' THEN 1 END), \
           COUNT(CASE WHEN s.status = 'unsubscribed' THEN 1 END) \
         FROM sites si JOIN lists l ON l.site_id = si.id LEFT JOIN subscriptions s ON s.list_id = l.id \
         WHERE si.account_id = $1 GROUP BY si.id, si.name, l.id, l.name ORDER BY si.name, l.name",
    )
    .bind(u.account_id)
    .fetch_all(&st.db)
    .await?;
    let lists: Vec<Value> = rows
        .into_iter()
        .map(
            |(site_id, site_name, id, name, confirmed, pending, unsubscribed)| {
                json!({ "site_id": site_id, "site_name": site_name, "id": id, "name": name,
                    "confirmed": confirmed, "pending": pending, "unsubscribed": unsubscribed })
            },
        )
        .collect();
    let recent: Vec<Campaign> = sqlx::query_as(&format!(
        "SELECT {} FROM campaigns c WHERE c.list_id IN (SELECT l.id FROM lists l JOIN sites si ON si.id = l.site_id WHERE si.account_id = $1) \
         ORDER BY c.id DESC LIMIT 10",
        CAMPAIGN_COLS.split(", ").map(|c| format!("c.{c}")).collect::<Vec<_>>().join(", ")
    ))
    .bind(u.account_id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(json!({ "lists": lists, "recent_campaigns": recent })))
}

// ---------------------------------------------------------------------------------------
// Sites

#[derive(Deserialize)]
pub struct SiteInput {
    pub name: String,
    pub domain: String,
    pub from_name: String,
    pub from_email: String,
    #[serde(default)]
    pub reply_to: String,
    #[serde(default)]
    pub physical_address: String,
    #[serde(default = "default_provider")]
    pub provider_kind: String,
    #[serde(default)]
    pub provider_config: Value,
    #[serde(default = "default_rate")]
    pub send_rate: i64,
}
fn default_provider() -> String {
    "log".into()
}
fn default_rate() -> i64 {
    10
}

impl SiteInput {
    fn validate(&mut self) -> AppResult<()> {
        self.name = self.name.trim().to_string();
        self.domain = self
            .domain
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_lowercase();
        self.from_email = self.from_email.trim().to_lowercase();
        self.reply_to = self.reply_to.trim().to_string();
        if self.name.is_empty() || self.domain.is_empty() {
            return Err(AppError::bad_request("name and domain are required"));
        }
        if !valid_email(&self.from_email) {
            return Err(AppError::bad_request(
                "from_email is not a valid email address",
            ));
        }
        if !self.reply_to.is_empty() && !valid_email(&self.reply_to) {
            return Err(AppError::bad_request(
                "reply_to is not a valid email address",
            ));
        }
        if !["log", "ses", "smtp"].contains(&self.provider_kind.as_str()) {
            return Err(AppError::bad_request(
                "provider must be one of log, ses, smtp",
            ));
        }
        if !self.provider_config.is_object() {
            self.provider_config = json!({});
        }
        self.send_rate = self.send_rate.clamp(1, 500);
        Ok(())
    }

    /// Keeps existing secret values when the client sends back the masked placeholder or blank.
    fn merge_secrets(&mut self, existing: &str) {
        let old: Value = serde_json::from_str(existing).unwrap_or_default();
        if let Some(obj) = self.provider_config.as_object_mut() {
            for k in SECRET_KEYS {
                let incoming = obj.get(*k).and_then(|v| v.as_str()).unwrap_or("");
                if incoming.is_empty() || incoming == "********" {
                    if let Some(prev) = old.get(*k) {
                        obj.insert(k.to_string(), prev.clone());
                    }
                }
            }
        }
    }
}

pub async fn list_sites(State(st): State<AppState>, u: AuthUser) -> J {
    let sites: Vec<Site> = sqlx::query_as(&format!(
        "SELECT {SITE_COLS} FROM sites WHERE account_id = $1 ORDER BY name"
    ))
    .bind(u.account_id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(Value::Array(
        sites
            .iter()
            .map(|s| site_json(s, &st.cfg.base_url))
            .collect(),
    )))
}

pub async fn get_site(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    Ok(Json(site_json(
        &site_for(&st, &u, id).await?,
        &st.cfg.base_url,
    )))
}

pub async fn create_site(
    State(st): State<AppState>,
    u: AuthUser,
    Json(mut i): Json<SiteInput>,
) -> J {
    i.validate()?;
    crate::crypto::seal_config(&mut i.provider_config)?;
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO sites (account_id, name, domain, from_name, from_email, reply_to, physical_address, provider_kind, \
         provider_config, send_rate, webhook_secret, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) RETURNING id",
    )
    .bind(u.account_id)
    .bind(&i.name)
    .bind(&i.domain)
    .bind(&i.from_name)
    .bind(&i.from_email)
    .bind(&i.reply_to)
    .bind(&i.physical_address)
    .bind(&i.provider_kind)
    .bind(i.provider_config.to_string())
    .bind(i.send_rate)
    .bind(random_token(24))
    .bind(now())
    .fetch_one(&st.db)
    .await?;
    crate::favicon::refresh_soon(&st, id, i.domain.clone()).await;
    get_site(State(st), u, Path(id)).await
}

/// Updates a site. The CASEs compare against the row's *current* domain inside the statement, so a
/// domain change atomically drops the old icon and marks it unchecked, whatever other saves race it.
pub(crate) const UPDATE_SITE_SQL: &str =
    "UPDATE sites SET name=$1, domain=$2, from_name=$3, from_email=$4, reply_to=$5, \
     physical_address=$6, provider_kind=$7, provider_config=$8, send_rate=$9, \
     icon_data = CASE WHEN domain = $2 THEN icon_data ELSE NULL END, \
     icon_type = CASE WHEN domain = $2 THEN icon_type ELSE NULL END, \
     icon_checked_at = CASE WHEN domain = $2 THEN icon_checked_at ELSE NULL END WHERE id=$10";

pub(crate) async fn site_needs_icon(db: &crate::db::Db, id: i64) -> AppResult<bool> {
    let (checked,): (Option<String>,) =
        sqlx::query_as("SELECT icon_checked_at FROM sites WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await?;
    Ok(checked.is_none())
}

pub async fn update_site(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(mut i): Json<SiteInput>,
) -> J {
    let existing = site_for(&st, &u, id).await?;
    i.validate()?;
    i.merge_secrets(&existing.provider_config);
    crate::crypto::seal_config(&mut i.provider_config)?;
    sqlx::query(UPDATE_SITE_SQL)
        .bind(&i.name)
        .bind(&i.domain)
        .bind(&i.from_name)
        .bind(&i.from_email)
        .bind(&i.reply_to)
        .bind(&i.physical_address)
        .bind(&i.provider_kind)
        .bind(i.provider_config.to_string())
        .bind(i.send_rate)
        .bind(id)
        .execute(&st.db)
        .await?;
    // Decide from the row as written (not our earlier read, which a concurrent save may have
    // made stale): an unchecked icon means the domain changed and the icon needs fetching.
    if site_needs_icon(&st.db, id).await? {
        let (domain,): (String,) = sqlx::query_as("SELECT domain FROM sites WHERE id = $1")
            .bind(id)
            .fetch_one(&st.db)
            .await?;
        crate::favicon::refresh_soon(&st, id, domain).await;
    }
    get_site(State(st), u, Path(id)).await
}

pub async fn delete_site(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    site_for(&st, &u, id).await?;
    sqlx::query("DELETE FROM sites WHERE id = $1")
        .bind(id)
        .execute(&st.db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct TestInput {
    pub email: String,
}

/// Sends a simple test email through the site's provider to verify configuration.
pub async fn test_site(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(t): Json<TestInput>,
) -> J {
    let site = site_for(&st, &u, id).await?;
    if !valid_email(t.email.trim()) {
        return Err(AppError::bad_request("invalid email"));
    }
    let msg = mailer::build_message(
        &site,
        &mailer::OutgoingEmail {
            to_email: t.email.trim().into(),
            to_name: String::new(),
            subject: format!("Listaculous test from {}", site.name),
            html: "<p>Your email provider is configured correctly. 🎉</p>".into(),
            text: "Your email provider is configured correctly.".into(),
            unsubscribe_url: None,
            list_id: None,
        },
    )
    .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    let provider = mailer::provider_for(&site, &st.http)
        .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    let id = provider
        .send(&msg)
        .await
        .map_err(|e| AppError::bad_request(format!("send failed: {e:#}")))?;
    Ok(Json(json!({ "ok": true, "message_id": id })))
}

pub async fn site_events(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    site_for(&st, &u, id).await?;
    let rows: Vec<(i64, String, String, String, String)> = sqlx::query_as(
        "SELECT id, email, kind, detail, created_at FROM email_events WHERE site_id = $1 ORDER BY id DESC LIMIT 200",
    )
    .bind(id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(Value::Array(
        rows.into_iter()
            .map(|(id, email, kind, detail, created_at)| json!({ "id": id, "email": email, "kind": kind, "detail": detail, "created_at": created_at }))
            .collect(),
    )))
}

// ---------------------------------------------------------------------------------------
// Lists

#[derive(Deserialize)]
pub struct ListInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "yes")]
    pub double_opt_in: bool,
    #[serde(default)]
    pub redirect_url: String,
}
fn yes() -> bool {
    true
}

fn list_json(l: &List, base_url: &str) -> Value {
    let mut v = serde_json::to_value(l).unwrap();
    v["subscribe_url"] = format!("{base_url}/subscribe/{}", l.public_id).into();
    v
}

pub async fn list_lists(State(st): State<AppState>, u: AuthUser, Path(site_id): Path<i64>) -> J {
    site_for(&st, &u, site_id).await?;
    let lists: Vec<List> = sqlx::query_as(&format!(
        "SELECT {LIST_COLS} FROM lists WHERE site_id = $1 ORDER BY name"
    ))
    .bind(site_id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(Value::Array(
        lists
            .iter()
            .map(|l| list_json(l, &st.cfg.base_url))
            .collect(),
    )))
}

fn validate_list(i: &ListInput) -> AppResult<()> {
    if i.name.trim().is_empty() {
        return Err(AppError::bad_request("name is required"));
    }
    if !i.redirect_url.is_empty()
        && !(i.redirect_url.starts_with("https://") || i.redirect_url.starts_with("http://"))
    {
        return Err(AppError::bad_request(
            "redirect URL must start with http:// or https://",
        ));
    }
    Ok(())
}

pub async fn create_list(
    State(st): State<AppState>,
    u: AuthUser,
    Path(site_id): Path<i64>,
    Json(i): Json<ListInput>,
) -> J {
    site_for(&st, &u, site_id).await?;
    validate_list(&i)?;
    let mut buf = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO lists (site_id, public_id, name, description, double_opt_in, redirect_url, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(site_id)
    .bind(hex::encode(buf))
    .bind(i.name.trim())
    .bind(&i.description)
    .bind(i.double_opt_in as i64)
    .bind(&i.redirect_url)
    .bind(now())
    .fetch_one(&st.db)
    .await?;
    get_list(State(st), u, Path(id)).await
}

pub async fn get_list(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    let (list, site) = list_for(&st, &u, id).await?;
    let mut v = list_json(&list, &st.cfg.base_url);
    v["site"] = site_json(&site, &st.cfg.base_url);
    Ok(Json(v))
}

pub async fn update_list(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(i): Json<ListInput>,
) -> J {
    list_for(&st, &u, id).await?;
    validate_list(&i)?;
    sqlx::query(
        "UPDATE lists SET name=$1, description=$2, double_opt_in=$3, redirect_url=$4 WHERE id=$5",
    )
    .bind(i.name.trim())
    .bind(&i.description)
    .bind(i.double_opt_in as i64)
    .bind(&i.redirect_url)
    .bind(id)
    .execute(&st.db)
    .await?;
    get_list(State(st), u, Path(id)).await
}

pub async fn delete_list(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    list_for(&st, &u, id).await?;
    sqlx::query("DELETE FROM lists WHERE id = $1")
        .bind(id)
        .execute(&st.db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------------------
// Subscribers

#[derive(Deserialize)]
pub struct SubQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub page: i64,
}

pub async fn list_subscribers(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<SubQuery>,
) -> J {
    list_for(&st, &u, id).await?;
    let pattern = format!("%{}%", q.q.trim().to_lowercase());
    let page = q.page.max(0);
    let rows: Vec<(i64, String, String, String, String, String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT s.id, sb.email, sb.name, s.status, sb.status, s.consent_source, s.created_at, s.confirmed_at, s.unsubscribed_at \
         FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id \
         WHERE s.list_id = $1 AND (sb.email LIKE $2 OR LOWER(sb.name) LIKE $2) AND ($3 = '' OR s.status = $3) \
         ORDER BY s.id DESC LIMIT 50 OFFSET $4",
    )
    .bind(id)
    .bind(&pattern)
    .bind(&q.status)
    .bind(page * 50)
    .fetch_all(&st.db)
    .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id \
         WHERE s.list_id = $1 AND (sb.email LIKE $2 OR LOWER(sb.name) LIKE $2) AND ($3 = '' OR s.status = $3)",
    )
    .bind(id)
    .bind(&pattern)
    .bind(&q.status)
    .fetch_one(&st.db)
    .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(id, email, name, status, subscriber_status, source, created_at, confirmed_at, unsubscribed_at)| {
            json!({ "id": id, "email": email, "name": name, "status": status, "subscriber_status": subscriber_status,
                    "source": source, "created_at": created_at, "confirmed_at": confirmed_at, "unsubscribed_at": unsubscribed_at })
        })
        .collect();
    Ok(Json(
        json!({ "items": items, "total": total, "page": page, "page_size": 50 }),
    ))
}

#[derive(Deserialize)]
pub struct AddSubscriber {
    pub email: String,
    #[serde(default)]
    pub name: String,
}

/// Manually adds a subscriber as confirmed. The admin attests they have consent.
pub async fn add_subscriber(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(a): Json<AddSubscriber>,
) -> J {
    let (list, _) = list_for(&st, &u, id).await?;
    let email = a.email.trim().to_lowercase();
    if !valid_email(&email) {
        return Err(AppError::bad_request("invalid email"));
    }
    let outcome = upsert_subscription(&st, &list, &email, a.name.trim(), "", "admin", true).await?;
    let result = match outcome {
        crate::public::SubscribeOutcome::Suppressed => "suppressed",
        crate::public::SubscribeOutcome::AlreadyConfirmed => "already_subscribed",
        _ => "added",
    };
    Ok(Json(json!({ "result": result })))
}

#[derive(Deserialize)]
pub struct ImportInput {
    pub csv: String,
}

/// Imports `email[,name]` lines as confirmed subscribers. Suppressed addresses are skipped.
pub async fn import_subscribers(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(i): Json<ImportInput>,
) -> J {
    let (list, _) = list_for(&st, &u, id).await?;
    let (mut added, mut skipped, mut invalid) = (0, 0, 0);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(i.csv.as_bytes());
    // Columns default to email,name; a header row (as produced by our export) remaps them.
    let (mut email_col, mut name_col, mut status_col) = (0usize, Some(1usize), None::<usize>);
    for (n, rec) in rdr.records().enumerate() {
        let Ok(rec) = rec else {
            invalid += 1;
            continue;
        };
        if n == 0 && rec.iter().any(|f| f.eq_ignore_ascii_case("email")) {
            let find = |k: &str| rec.iter().position(|f| f.eq_ignore_ascii_case(k));
            email_col = find("email").unwrap_or(0);
            name_col = find("name");
            status_col = find("status");
            continue;
        }
        let email = rec.get(email_col).unwrap_or("").to_lowercase();
        let name = name_col.and_then(|c| rec.get(c)).unwrap_or("").to_string();
        if email.is_empty() {
            continue;
        }
        // Never re-subscribe rows exported as unsubscribed/pending.
        if status_col
            .and_then(|c| rec.get(c))
            .is_some_and(|s| !s.is_empty() && s != "confirmed")
        {
            skipped += 1;
            continue;
        }
        if !valid_email(&email) {
            invalid += 1;
            continue;
        }
        match upsert_subscription(&st, &list, &email, &name, "", "import", true).await? {
            crate::public::SubscribeOutcome::Confirmed => added += 1,
            _ => skipped += 1,
        }
    }
    Ok(Json(
        json!({ "added": added, "skipped": skipped, "invalid": invalid }),
    ))
}

pub async fn export_subscribers(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<impl IntoResponse> {
    let (list, _) = list_for(&st, &u, id).await?;
    let rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT sb.email, sb.name, s.status, s.created_at, s.confirmed_at FROM subscriptions s \
         JOIN subscribers sb ON sb.id = s.subscriber_id WHERE s.list_id = $1 ORDER BY s.id",
    )
    .bind(id)
    .fetch_all(&st.db)
    .await?;
    let esc = |s: &str| {
        // Quote fields and neutralize spreadsheet formula injection.
        let s = if s.starts_with(['=', '+', '-', '@']) {
            format!("'{s}")
        } else {
            s.to_string()
        };
        format!("\"{}\"", s.replace('"', "\"\""))
    };
    let mut out = String::from("email,name,status,created_at,confirmed_at\n");
    for (email, name, status, created, confirmed) in rows {
        // Emails are written verbatim (quoted) so exports re-import losslessly; only free text is defused.
        out.push_str(&format!(
            "\"{}\",{},{},{},{}\n",
            email.replace('"', "\"\""),
            esc(&name),
            status,
            created,
            confirmed.unwrap_or_default()
        ));
    }
    let filename = format!(
        "{}-subscribers.csv",
        list.name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
    );
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        out,
    ))
}

async fn subscription_owner(st: &AppState, u: &AuthUser, id: i64) -> AppResult<(i64, i64, String)> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT s.list_id, sb.email FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id WHERE s.id = $1",
    )
    .bind(id)
    .fetch_optional(&st.db)
    .await?;
    let (list_id, email) = row.ok_or_else(AppError::not_found)?;
    let (_, site) = list_for(st, u, list_id).await?;
    Ok((list_id, site.id, email))
}

pub async fn delete_subscription(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
) -> J {
    subscription_owner(&st, &u, id).await?;
    sqlx::query("DELETE FROM subscriptions WHERE id = $1")
        .bind(id)
        .execute(&st.db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
pub struct AdminUnsub {
    #[serde(default)]
    pub suppress: bool,
}

pub async fn admin_unsubscribe(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(a): Json<AdminUnsub>,
) -> J {
    let (_, site_id, email) = subscription_owner(&st, &u, id).await?;
    if a.suppress {
        suppress(&st, site_id, &email, "suppressed").await?;
    } else {
        sqlx::query(
            "UPDATE subscriptions SET status = 'unsubscribed', unsubscribed_at = $1 WHERE id = $2",
        )
        .bind(now())
        .bind(id)
        .execute(&st.db)
        .await?;
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------------------
// Campaigns

#[derive(Deserialize)]
pub struct CampaignInput {
    pub subject: String,
    #[serde(default)]
    pub preheader: String,
    #[serde(default)]
    pub html_body: String,
    #[serde(default)]
    pub text_body: String,
}

async fn campaign_json(st: &AppState, c: &Campaign) -> AppResult<Value> {
    let (total, sent, failed, skipped): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(CASE WHEN status = 'sent' THEN 1 END), COUNT(CASE WHEN status = 'failed' THEN 1 END), \
         COUNT(CASE WHEN status = 'skipped' THEN 1 END) FROM campaign_sends WHERE campaign_id = $1",
    )
    .bind(c.id)
    .fetch_one(&st.db)
    .await?;
    let mut v = serde_json::to_value(c).unwrap();
    v["stats"] = json!({ "total": total, "sent": sent, "failed": failed, "skipped": skipped });
    Ok(v)
}

pub async fn list_campaigns(
    State(st): State<AppState>,
    u: AuthUser,
    Path(list_id): Path<i64>,
) -> J {
    list_for(&st, &u, list_id).await?;
    let cs: Vec<Campaign> = sqlx::query_as(&format!(
        "SELECT {CAMPAIGN_COLS} FROM campaigns WHERE list_id = $1 ORDER BY id DESC"
    ))
    .bind(list_id)
    .fetch_all(&st.db)
    .await?;
    let mut out = Vec::with_capacity(cs.len());
    for c in &cs {
        out.push(campaign_json(&st, c).await?);
    }
    Ok(Json(Value::Array(out)))
}

pub async fn create_campaign(
    State(st): State<AppState>,
    u: AuthUser,
    Path(list_id): Path<i64>,
    Json(i): Json<CampaignInput>,
) -> J {
    list_for(&st, &u, list_id).await?;
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO campaigns (list_id, subject, preheader, html_body, text_body, status, created_at) \
         VALUES ($1,$2,$3,$4,$5,'draft',$6) RETURNING id",
    )
    .bind(list_id)
    .bind(i.subject.trim())
    .bind(&i.preheader)
    .bind(&i.html_body)
    .bind(&i.text_body)
    .bind(now())
    .fetch_one(&st.db)
    .await?;
    get_campaign(State(st), u, Path(id)).await
}

pub async fn get_campaign(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    let (c, _, _) = campaign_for(&st, &u, id).await?;
    Ok(Json(campaign_json(&st, &c).await?))
}

pub async fn update_campaign(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(i): Json<CampaignInput>,
) -> J {
    let (c, _, _) = campaign_for(&st, &u, id).await?;
    if c.status != "draft" {
        return Err(AppError::bad_request("only draft campaigns can be edited"));
    }
    let r = sqlx::query("UPDATE campaigns SET subject=$1, preheader=$2, html_body=$3, text_body=$4 WHERE id=$5 AND status='draft'")
        .bind(i.subject.trim())
        .bind(&i.preheader)
        .bind(&i.html_body)
        .bind(&i.text_body)
        .bind(id)
        .execute(&st.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::new(
            axum::http::StatusCode::CONFLICT,
            "campaign is no longer a draft; changes were not saved",
        ));
    }
    get_campaign(State(st), u, Path(id)).await
}

pub async fn delete_campaign(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    let (c, _, _) = campaign_for(&st, &u, id).await?;
    if c.status == "sending" {
        return Err(AppError::bad_request(
            "pause the campaign before deleting it",
        ));
    }
    let r = sqlx::query("DELETE FROM campaigns WHERE id = $1 AND status <> 'sending'")
        .bind(id)
        .execute(&st.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::bad_request(
            "pause the campaign before deleting it",
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

fn sample_recipient<'a>(email: &'a str) -> Recipient<'a> {
    Recipient {
        email,
        name: "Friend",
        token: "preview",
    }
}

pub async fn preview_campaign(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(i): Json<CampaignInput>,
) -> J {
    let (mut c, list, site) = campaign_for(&st, &u, id).await?;
    c.subject = i.subject;
    c.preheader = i.preheader;
    c.html_body = i.html_body;
    c.text_body = i.text_body;
    let e = render_campaign(
        &st.cfg.base_url,
        &site,
        &list,
        &c,
        &sample_recipient("subscriber@example.com"),
    );
    Ok(Json(
        json!({ "subject": e.subject, "html": e.html, "text": e.text }),
    ))
}

pub async fn test_campaign(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(t): Json<TestInput>,
) -> J {
    let (c, list, site) = campaign_for(&st, &u, id).await?;
    let to = t.email.trim().to_lowercase();
    if !valid_email(&to) {
        return Err(AppError::bad_request("invalid email"));
    }
    let mut e = render_campaign(&st.cfg.base_url, &site, &list, &c, &sample_recipient(&to));
    e.subject = format!("[TEST] {}", e.subject);
    let msg =
        mailer::build_message(&site, &e).map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    let provider = mailer::provider_for(&site, &st.http)
        .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    provider
        .send(&msg)
        .await
        .map_err(|e| AppError::bad_request(format!("send failed: {e:#}")))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn send_campaign(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    let (c, list, site) = campaign_for(&st, &u, id).await?;
    if c.subject.trim().is_empty() || c.html_body.trim().is_empty() {
        return Err(AppError::bad_request("subject and body are required"));
    }
    if site.physical_address.trim().is_empty() {
        return Err(AppError::bad_request(
            "set a physical mailing address on the site first (required by CAN-SPAM and most providers)",
        ));
    }
    mailer::provider_for(&site, &st.http).map_err(|e| AppError::bad_request(format!("{e:#}")))?;

    let mut tx = st.db.begin().await?;
    let res = match c.status.as_str() {
        "draft" => {
            let r = sqlx::query("UPDATE campaigns SET status = 'sending', started_at = $1 WHERE id = $2 AND status = 'draft' \
                 AND TRIM(subject) <> '' AND TRIM(html_body) <> ''")
                .bind(now())
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO campaign_sends (campaign_id, subscription_id, status) \
                 SELECT $1, s.id, 'queued' FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id \
                 WHERE s.list_id = $2 AND s.status = 'confirmed' AND sb.status = 'active'",
            )
            .bind(id)
            .bind(list.id)
            .execute(&mut *tx)
            .await?;
            r
        }
        // Resume a paused campaign; already-queued rows are picked up by the worker.
        "paused" | "sent" => {
            // Give failed recipients a fresh set of attempts, except those whose delivery is unknown
            // (they may have been delivered, so they are never retried automatically).
            sqlx::query(
                "UPDATE campaign_sends SET status = 'queued', attempts = 0, next_attempt_at = NULL WHERE campaign_id = $1 AND status = 'failed' \
                 AND error NOT LIKE 'unknown:%'",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            let (pending,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM campaign_sends WHERE campaign_id = $1 AND status = 'queued'",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if c.status == "sent" && pending == 0 {
                return Err(AppError::bad_request("no failed recipients to retry"));
            }
            sqlx::query("UPDATE campaigns SET status = 'sending', finished_at = NULL WHERE id = $1 AND status = $2")
                .bind(id)
                .bind(&c.status)
                .execute(&mut *tx)
                .await?
        }
        _ => return Err(AppError::bad_request("campaign is already sending")),
    };
    if res.rows_affected() == 0 {
        return Err(AppError::bad_request(
            "campaign state changed; reload and try again",
        ));
    }
    tx.commit().await?;
    get_campaign(State(st), u, Path(id)).await
}

pub async fn pause_campaign(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    campaign_for(&st, &u, id).await?;
    sqlx::query("UPDATE campaigns SET status = 'paused' WHERE id = $1 AND status = 'sending'")
        .bind(id)
        .execute(&st.db)
        .await?;
    get_campaign(State(st), u, Path(id)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn update(db: &crate::db::Db, domain: &str) {
        sqlx::query(UPDATE_SITE_SQL)
            .bind("Site")
            .bind(domain)
            .bind("F")
            .bind("f@example.com")
            .bind("")
            .bind("1 St")
            .bind("log")
            .bind("{}")
            .bind(10_i64)
            .bind(1_i64)
            .execute(db)
            .await
            .unwrap();
    }

    /// Regression: request A read domain a.example; B changed it to b.example; then A saved its
    /// stale a.example. The icon must be cleared *and* flagged for fetching, not silently lost.
    #[tokio::test]
    async fn domain_change_flags_icon_even_with_stale_concurrent_save() {
        let path = std::env::temp_dir().join(format!("lc-test-{}.db", crate::db::random_token(8)));
        let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query("INSERT INTO accounts (name, created_at) VALUES ('a', 'now')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sites (account_id, name, domain, from_name, from_email, webhook_secret, created_at, icon_data, icon_type, icon_checked_at) \
             VALUES (1, 'Site', 'a.example', 'F', 'f@example.com', 's', 'now', $1, 'image/png', 't0')",
        )
        .bind(vec![1u8, 2, 3])
        .execute(&db).await.unwrap();

        update(&db, "a.example").await; // unrelated save: icon kept
        assert!(!site_needs_icon(&db, 1).await.unwrap());

        update(&db, "b.example").await; // B: domain change
        assert!(site_needs_icon(&db, 1).await.unwrap());
        sqlx::query(
            "UPDATE sites SET icon_type = 'image/png', icon_checked_at = 't1' WHERE id = 1",
        )
        .execute(&db)
        .await
        .unwrap(); // B's fetch done

        update(&db, "a.example").await; // A: stale save restores the old domain
        assert!(
            site_needs_icon(&db, 1).await.unwrap(),
            "icon for restored domain must be re-fetched"
        );
        let (t,): (Option<String>,) = sqlx::query_as("SELECT icon_type FROM sites WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(t.is_none(), "b.example's icon must not remain on a.example");
        let _ = std::fs::remove_file(path);
    }
}
