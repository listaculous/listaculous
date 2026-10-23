//! Fetches a site's favicon from its domain and stores it in the database, so the admin UI can show
//! it without hot-linking the site or using a third-party favicon service.
//!
//! The domain is user input, so fetching is treated as untrusted (SSRF-safe):
//! - every hop (including redirects) must resolve only to public IP addresses, and the request is
//!   pinned to the address we checked, so DNS rebinding can't swap in an internal one;
//! - responses are size- and time-limited, and images are identified by their bytes, not headers;
//! - icons are served back with a sandboxing CSP so a malicious SVG can't run script.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use url::Url;

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::AppState;

const MAX_HTML: usize = 512 * 1024;
const MAX_ICON: usize = 256 * 1024;
const MAX_REDIRECTS: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(6);

/// One lock per site so overlapping refreshes (automatic + manual) run in order and the newest
/// result is always the one stored.
static LOCKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(Default::default);

/// Fetches and stores the icon for a site. Never fails the caller: errors just mean "no icon".
pub async fn refresh(st: &AppState, site_id: i64, domain: &str) {
    let lock = LOCKS.lock().unwrap().entry(site_id).or_default().clone();
    let _guard = lock.lock().await;
    let found = match find_icon(domain).await {
        Ok(icon) => Some(icon),
        Err(e) => {
            tracing::info!("no favicon for {domain}: {e:#}");
            None
        }
    };
    let (data, mime) = match found {
        Some((d, m)) => (Some(d), Some(m)),
        None => (None, None),
    };
    let res = sqlx::query("UPDATE sites SET icon_data = $1, icon_type = $2, icon_checked_at = $3 WHERE id = $4 AND domain = $5")
        .bind(data)
        .bind(mime)
        // Millisecond precision: this value is the icon URL's cache-busting version.
        .bind(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .bind(site_id)
        .bind(domain)
        .execute(&st.db)
        .await;
    if let Err(e) = res {
        tracing::error!("saving favicon for site {site_id}: {e}");
    }
}

/// Fetches the icon, waiting briefly so the saved site usually comes back with its icon; slow sites
/// finish in the background (and show up on the next load).
pub async fn refresh_soon(st: &AppState, site_id: i64, domain: String) {
    let st = st.clone();
    let task = tokio::spawn(async move { refresh(&st, site_id, &domain).await });
    let _ = tokio::time::timeout(Duration::from_secs(4), task).await;
}

/// Fetches icons for sites that have never been checked (e.g. created before this feature).
pub async fn backfill(st: AppState) {
    let rows: Vec<(i64, String)> =
        match sqlx::query_as("SELECT id, domain FROM sites WHERE icon_checked_at IS NULL")
            .fetch_all(&st.db)
            .await
        {
            Ok(r) => r,
            Err(e) => return tracing::error!("favicon backfill: {e}"),
        };
    for (id, domain) in rows {
        refresh(&st, id, &domain).await;
    }
}

async fn find_icon(domain: &str) -> anyhow::Result<(Vec<u8>, String)> {
    let domain = domain.trim().trim_end_matches('/');
    if domain.is_empty() || domain.contains(['/', '@', ' ', '?', '#']) {
        bail!("not a plain domain");
    }
    // Try the home page over HTTPS, then HTTP, and collect <link rel=icon> candidates.
    let mut candidates = Vec::new();
    let mut base = None;
    for scheme in ["https", "http"] {
        let url = Url::parse(&format!("{scheme}://{domain}/"))?;
        if let Ok((final_url, _, html)) = fetch(url, MAX_HTML).await {
            candidates = icon_links(&String::from_utf8_lossy(&html), &final_url);
            base = Some(final_url);
            break;
        }
    }
    let base = match base {
        Some(b) => b,
        None => Url::parse(&format!("https://{domain}/"))?,
    };
    candidates.push(base.join("/favicon.ico")?);

    for url in candidates {
        if let Ok((_, _, bytes)) = fetch(url, MAX_ICON).await {
            if let Some(mime) = sniff(&bytes) {
                return Ok((bytes, mime.to_string()));
            }
        }
    }
    bail!("no usable icon found")
}

/// Extracts icon links from HTML, best first (apple-touch-icon / larger / SVG before tiny icons).
fn icon_links(html: &str, base: &Url) -> Vec<Url> {
    let link_re = regex_lite::Regex::new(r"(?is)<link\b[^>]*>").unwrap();
    let attr_re = regex_lite::Regex::new(
        r#"(?is)([a-z][a-z0-9_-]*)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))"#,
    )
    .unwrap();
    let mut found: Vec<(f64, Url)> = Vec::new();
    for tag in link_re.find_iter(html) {
        let mut rel = String::new();
        let mut href = None;
        let mut sizes = String::new();
        let mut kind = String::new();
        for a in attr_re.captures_iter(tag.as_str()) {
            let v = a
                .get(2)
                .or(a.get(3))
                .or(a.get(4))
                .map(|m| m.as_str().trim())
                .unwrap_or("");
            match a[1].to_ascii_lowercase().as_str() {
                "rel" => rel = v.to_ascii_lowercase(),
                "href" => href = Some(v.to_string()),
                "sizes" => sizes = v.to_ascii_lowercase(),
                "type" => kind = v.to_ascii_lowercase(),
                _ => {}
            }
        }
        if !rel
            .split_whitespace()
            .any(|r| r == "icon" || r.starts_with("apple-touch-icon"))
        {
            continue;
        }
        let Some(url) = href.and_then(|h| base.join(&h).ok()) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        let largest = sizes
            .split_whitespace()
            .filter_map(|s| s.split('x').next()?.parse::<f64>().ok())
            .fold(0.0, f64::max);
        let mut score = if rel.contains("apple-touch-icon") {
            3.0
        } else {
            1.0
        };
        if kind.contains("svg") || url.path().ends_with(".svg") {
            score += 1.5;
        }
        score += largest.min(512.0) / 512.0;
        found.push((score, url));
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    found.into_iter().map(|(_, u)| u).take(4).collect()
}

/// Identifies supported image formats by their leading bytes.
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.starts_with(&[0, 0, 1, 0]) {
        Some("image/x-icon")
    } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        let head = String::from_utf8_lossy(&b[..b.len().min(1024)]).to_ascii_lowercase();
        let head = head.trim_start_matches('\u{feff}').trim_start();
        (head.starts_with("<svg") || (head.starts_with("<?xml") && head.contains("<svg")))
            .then_some("image/svg+xml")
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1])) // carrier-grade NAT
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // benchmarking
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique local
                || (seg[0] & 0xffc0) == 0xfe80 // link local
                || seg[0] == 0x2001 && seg[1] == 0x0db8) // documentation
        }
    }
}

/// Resolves the URL's host and returns one public address to pin the request to.
async fn public_addr(url: &Url) -> anyhow::Result<SocketAddr> {
    let host = url.host_str().ok_or_else(|| anyhow!("no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("no port"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> =
        tokio::time::timeout(TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .context("dns timeout")??
            .collect();
    if addrs.is_empty() {
        bail!("{host} does not resolve");
    }
    // Reject if *any* address is internal, so a mixed record can't be used to reach one.
    if let Some(bad) = addrs.iter().find(|a| !is_public(a.ip())) {
        bail!("{host} resolves to a non-public address ({})", bad.ip());
    }
    Ok(addrs[0])
}

/// GETs a URL following up to MAX_REDIRECTS redirects, validating every hop, with a body limit.
async fn fetch(mut url: Url, max: usize) -> anyhow::Result<(Url, Option<String>, Vec<u8>)> {
    for _ in 0..=MAX_REDIRECTS {
        if !matches!(url.scheme(), "http" | "https") {
            bail!("unsupported scheme");
        }
        let addr = public_addr(&url).await?;
        let host = url.host_str().unwrap_or_default().to_string();
        // Pin this request to the address we just validated.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // A proxy would resolve the hostname itself, bypassing the pinned address check.
            .no_proxy()
            .timeout(TIMEOUT)
            .resolve(&host, addr)
            .user_agent("Listaculous favicon fetcher (+https://github.com/listaculous/listaculous)")
            .build()?;
        let mut resp = client.get(url.clone()).send().await?;
        if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("redirect without location"))?;
            url = url.join(loc)?;
            continue;
        }
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        if resp.content_length().is_some_and(|n| n as usize > max) {
            bail!("too large");
        }
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if body.len() + chunk.len() > max {
                bail!("too large");
            }
            body.extend_from_slice(&chunk);
        }
        return Ok((url, ctype, body));
    }
    bail!("too many redirects")
}

// ---------------------------------------------------------------------------------------
// HTTP handlers

pub async fn get_icon(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let row: Option<(Option<Vec<u8>>, Option<String>)> =
        sqlx::query_as("SELECT icon_data, icon_type FROM sites WHERE id = $1 AND account_id = $2")
            .bind(id)
            .bind(u.account_id)
            .fetch_optional(&st.db)
            .await?;
    let Some((Some(data), Some(mime))) = row else {
        return Err(AppError::not_found());
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            // A fetched SVG is untrusted: never let it run script on our origin.
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; sandbox".to_string(),
            ),
        ],
        data,
    )
        .into_response())
}

pub async fn refresh_icon(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT domain FROM sites WHERE id = $1 AND account_id = $2")
            .bind(id)
            .bind(u.account_id)
            .fetch_optional(&st.db)
            .await?;
    let (domain,) = row.ok_or_else(AppError::not_found)?;
    refresh(&st, id, &domain).await;
    Ok(crate::api::get_site(State(st), u, Path(id))
        .await?
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_internal_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn ranks_icon_links() {
        let base = Url::parse("https://example.com/blog/").unwrap();
        let html = r#"<head>
            <link rel="icon" href="/favicon-16.png" sizes="16x16">
            <link href='icons/touch.png' rel='apple-touch-icon' sizes='180x180'>
            <link rel="stylesheet" href="/site.css">
            <LINK REL="shortcut icon" HREF="//cdn.example.com/f.ico">
            <link rel="icon" href="javascript:alert(1)">
        </head>"#;
        let links: Vec<String> = icon_links(html, &base)
            .into_iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(links[0], "https://example.com/blog/icons/touch.png");
        assert!(links.contains(&"https://cdn.example.com/f.ico".to_string()));
        assert!(!links
            .iter()
            .any(|l| l.contains("css") || l.starts_with("javascript")));
    }

    #[test]
    fn sniffs_formats() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(&[0, 0, 1, 0, 1, 0]), Some("image/x-icon"));
        assert_eq!(
            sniff(b"  <svg xmlns='http://www.w3.org/2000/svg'></svg>"),
            Some("image/svg+xml")
        );
        assert_eq!(sniff(b"<!doctype html><html>"), None);
    }
}
