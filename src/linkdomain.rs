//! Custom link domains: a site's confirm/unsubscribe/signup links can live on its own hostname
//! (`Site::public_url`). This creates the CNAME pointing that hostname at this server through a
//! connected DNS provider, checks it serves Listaculous over HTTPS, and answers Caddy's on-demand
//! TLS `ask` so certificates for those hostnames are issued without editing the proxy config.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthUser;
use crate::dns::{same_content, Record};
use crate::error::{AppError, AppResult};
use crate::{integrations, AppState};

#[derive(Deserialize, Default)]
pub struct LinkDomainRequest {
    /// DNS integration to create the record with; omit to only check.
    #[serde(default)]
    pub dns_integration_id: Option<i64>,
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|h| h.to_ascii_lowercase())
}

/// Candidate zones for `host`, most likely first: the site's own domain when the host is under
/// it, then each parent with at least two labels from the shortest up (`example.com` before
/// `news.example.com`). The host itself is never a zone here, since a CNAME can't sit at an apex.
pub fn candidate_zones(host: &str, site_domain: &str) -> Vec<String> {
    let mut out = Vec::new();
    let d = site_domain.trim().to_ascii_lowercase();
    if !d.is_empty() && host.ends_with(&format!(".{d}")) {
        out.push(d);
    }
    let labels: Vec<&str> = host.split('.').collect();
    for i in (1..labels.len().saturating_sub(1)).rev() {
        let z = labels[i..].join(".");
        if !out.contains(&z) {
            out.push(z);
        }
    }
    out
}

/// `POST /api/sites/{id}/link-domain`: with `dns_integration_id`, adds the CNAME (never edits or
/// deletes records; refuses if the name already has a different record). Always reports whether
/// the link domain currently serves this server over HTTPS.
pub async fn setup(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    body: Option<Json<LinkDomainRequest>>,
) -> AppResult<Json<Value>> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let site = crate::api::site_for(&st, &u, id).await?;
    if site.public_url.is_empty() {
        return Err(AppError::bad_request(
            "set a link domain in the site's settings first",
        ));
    }
    let host =
        host_of(&site.public_url).ok_or_else(|| AppError::bad_request("invalid link domain"))?;
    let target = host_of(&st.cfg.base_url)
        .ok_or_else(|| AppError::bad_request("BASE_URL has no hostname"))?;
    if target == host {
        return Err(AppError::bad_request(
            "the link domain is this server's own hostname; nothing to set up",
        ));
    }
    let wanted = json!({ "fqdn": host, "type": "CNAME", "content": target });

    let mut dns = Value::Null;
    if let Some(dns_id) = req.dns_integration_id {
        let provider = integrations::load(&st, &u, dns_id).await?.dns(&st.http)?;
        let mut found = None;
        let mut last_err = String::new();
        for zone in candidate_zones(&host, &site.domain) {
            match provider.list(&zone).await {
                Ok(records) => {
                    found = Some((zone, records));
                    break;
                }
                Err(e) => last_err = format!("{e:#}"),
            }
        }
        let (zone, records) = found.ok_or_else(|| {
            AppError::bad_request(format!(
                "couldn't find {host}'s domain in that DNS account ({last_err}). In Porkbun, enable \"API Access\" for the domain."
            ))
        })?;
        let name = host
            .strip_suffix(&format!(".{zone}"))
            .unwrap_or_default()
            .to_string();
        let at_name: Vec<&Record> = records
            .iter()
            .filter(|r| r.name.eq_ignore_ascii_case(&name))
            .collect();
        let status = if at_name
            .iter()
            .any(|r| r.kind == "CNAME" && same_content("CNAME", &r.content, &target))
        {
            "exists"
        } else if let Some(r) = at_name.first() {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                format!(
                    "{host} already has a {} record ({}). Remove it or point it at {target}, then check again.",
                    r.kind, r.content
                ),
            ));
        } else {
            let rec = Record {
                id: String::new(),
                name: name.clone(),
                kind: "CNAME".into(),
                content: target.clone(),
                priority: None,
                ttl: Some(600),
            };
            provider
                .create(&zone, &rec)
                .await
                .map_err(|e| AppError::bad_request(format!("creating the record failed: {e:#}")))?;
            tracing::info!("link domain: created CNAME {host} -> {target} for site {id}");
            "created"
        };
        dns = json!({ "zone": zone, "status": status });
    }

    let (resolves, reachable, detail) = check(&host, &site.public_url).await;
    Ok(Json(json!({
        "host": host,
        "record": wanted,
        "dns": dns,
        "resolves": resolves,
        "reachable": reachable,
        "detail": detail,
    })))
}

/// Random per-process value served at `/healthz/instance`, so the check can tell this server apart
/// from any other that answers `/healthz` (another install, a proxy's own health endpoint).
pub fn instance_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| crate::db::random_token(24))
}

pub async fn instance() -> &'static str {
    instance_id()
}

/// Whether `host` resolves, and whether `{origin}/healthz/instance` is answered by this very server.
/// The request goes only to a validated public address (pinned, no proxy), never follows
/// redirects, and reads at most a few bytes, like the favicon fetcher.
async fn check(host: &str, origin: &str) -> (bool, bool, String) {
    const MAX: usize = 256;
    let Ok(url) = url::Url::parse(&format!("{origin}/healthz/instance")) else {
        return (false, false, "invalid link domain".into());
    };
    let resolves =
        match tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, 443)))
            .await
        {
            Ok(Ok(mut addrs)) => addrs.next().is_some(),
            _ => false,
        };
    if !resolves {
        return (
            false,
            false,
            format!("{host} doesn't resolve yet; new DNS records can take a few minutes"),
        );
    }
    let addr = match crate::favicon::public_addr(&url).await {
        Ok(a) => a,
        Err(e) => return (true, false, format!("{e:#}")),
    };
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .resolve(host, addr)
        .build()
    {
        Ok(c) => c,
        Err(e) => return (true, false, format!("{e:#}")),
    };
    let not_here = || {
        format!("{host} answers, but not from this Listaculous server; check your reverse proxy sends it here")
    };
    let mut resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                true,
                false,
                format!("couldn't reach {origin} ({e}); the HTTPS certificate may still be issuing, or your reverse proxy doesn't know {host} yet"),
            )
        }
    };
    if !resp.status().is_success() {
        return (
            true,
            false,
            format!(
                "{origin} returned {}; check your reverse proxy sends {host} here",
                resp.status()
            ),
        );
    }
    let mut body = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(c)) if body.len() + c.len() <= MAX => body.extend_from_slice(&c),
            Ok(None) => break,
            _ => return (true, false, not_here()),
        }
    }
    if body == instance_id().as_bytes() {
        (true, true, String::new())
    } else {
        (true, false, not_here())
    }
}

/// Records the host of a newly saved link domain so `/tls/ask` keeps approving it after the
/// setting changes (old emails still link there).
pub async fn remember(db: &crate::db::Db, site_id: i64, public_url: &str) -> AppResult<()> {
    if let Some(host) = host_of(public_url) {
        sqlx::query(
            "INSERT INTO link_domains (host, site_id, created_at) VALUES ($1, $2, $3) \
             ON CONFLICT (host, site_id) DO NOTHING",
        )
        .bind(host)
        .bind(site_id)
        .bind(crate::db::now())
        .execute(db)
        .await?;
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct AskQuery {
    #[serde(default)]
    domain: String,
}

/// `GET /tls/ask?domain=…`: Caddy's on-demand TLS permission check. 200 only for hostnames some
/// site uses or has used as its link domain (or this server's own), so nobody can make the proxy request
/// certificates for arbitrary names.
pub async fn tls_ask(State(st): State<AppState>, Query(q): Query<AskQuery>) -> StatusCode {
    let d = q.domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if d.is_empty()
        || !d
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return StatusCode::NOT_FOUND;
    }
    if host_of(&st.cfg.base_url).as_deref() == Some(d.as_str()) {
        return StatusCode::OK;
    }
    let found: Result<Option<(i64,)>, _> =
        // The current setting counts too, so approval never depends on the history row's write.
        sqlx::query_as(
            "SELECT site_id FROM link_domains WHERE host = $1 \
             UNION SELECT id FROM sites WHERE public_url = $2 LIMIT 1",
        )
        .bind(&d)
        .bind(format!("https://{d}"))
        .fetch_optional(&st.db)
            .await;
    match found {
        Ok(Some(_)) => StatusCode::OK,
        _ => StatusCode::NOT_FOUND,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_try_site_domain_then_parents() {
        assert_eq!(
            candidate_zones("newsletter.latterdayblog.com", "latterdayblog.com"),
            vec!["latterdayblog.com"]
        );
        assert_eq!(
            candidate_zones("a.b.example.co.uk", "other.com"),
            vec!["co.uk", "example.co.uk", "b.example.co.uk"]
        );
        assert!(candidate_zones("example.com", "example.com").is_empty());
    }

    /// Changing or clearing a site's link domain keeps earlier hosts approved for TLS; deleting
    /// the site retires them.
    #[tokio::test]
    async fn previous_link_domains_stay_approved() {
        let path = std::env::temp_dir().join(format!("lc-test-{}.db", crate::db::random_token(8)));
        let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query("INSERT INTO accounts (name, created_at) VALUES ('a', 'now')")
            .execute(&db)
            .await
            .unwrap();
        for _ in 0..2 {
            sqlx::query("INSERT INTO sites (account_id, name, domain, from_name, from_email, webhook_secret, created_at) \
                         VALUES (1, 'S', 'example.com', 'F', 'f@example.com', 's', 'now')")
                .execute(&db)
                .await
                .unwrap();
        }
        let approved = |h: &'static str| {
            let db = db.clone();
            async move {
                sqlx::query_as::<_, (i64,)>(
                    "SELECT site_id FROM link_domains WHERE host = $1 LIMIT 1",
                )
                .bind(h)
                .fetch_optional(&db)
                .await
                .unwrap()
                .is_some()
            }
        };
        remember(&db, 1, "https://a.example.com").await.unwrap();
        remember(&db, 1, "https://b.example.com").await.unwrap(); // changed
        remember(&db, 1, "").await.unwrap(); // cleared
        assert!(approved("a.example.com").await && approved("b.example.com").await);
        sqlx::query("DELETE FROM sites WHERE id = 1")
            .execute(&db)
            .await
            .unwrap();
        assert!(!approved("a.example.com").await);
        let _ = std::fs::remove_file(path);
    }
}
