//! Open and click tracking for campaign and automation emails.
//!
//! Each distinct http(s) link in a campaign's HTML gets a row in `links`; the email links to
//! `{link domain}/c/{link key}/{subscription token}` instead, which records the click and redirects
//! to the stored URL (only ever a URL from the campaign itself, so it's not an open redirect). A
//! 1×1 image at `/o/{campaign id}/{token}` records opens. Opens are approximate: some mail apps
//! fetch images for privacy whether or not the email was read, and others block them. Links whose
//! URL contains a {{placeholder}} differ per recipient and are left as they are, as are the
//! unsubscribe link and the plain-text part.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use regex_lite::Regex;

use crate::db::{now, Db};
use crate::mailer::OutgoingEmail;
use crate::AppState;

fn href_re() -> Regex {
    // Double-quoted, single-quoted or unquoted attribute values.
    Regex::new(
        r#"(?i)(href\s*=\s*)("(https?://[^"]*)"|'(https?://[^']*)'|(https?://[^\s"'<>=`]+))"#,
    )
    .unwrap()
}

/// The URL an href attribute value stands for: decodes numeric (`&#38;`, `&#x26;`) and the common
/// named character references. Anything else is kept as written.
fn decode_attr(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[1..].find(';').map(|j| j + 1).filter(|&j| j <= 12);
        let decoded = end.and_then(|j| {
            let name = &rest[1..j];
            let c = match name {
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                _ => {
                    let num = name.strip_prefix('#')?;
                    let n = match num.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok()?,
                        None => num.parse().ok()?,
                    };
                    char::from_u32(n)
                }
            }?;
            Some((c, j + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Registers the trackable links in a campaign's HTML; returns attribute value → link key.
pub async fn links_for(
    db: &Db,
    campaign_id: i64,
    html_body: &str,
) -> anyhow::Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for cap in href_re().captures_iter(html_body) {
        let raw = cap
            .get(3)
            .or(cap.get(4))
            .or(cap.get(5))
            .map_or("", |m| m.as_str());
        if raw.is_empty() || raw.contains("{{") || raw.len() > 2000 || out.contains_key(raw) {
            continue;
        }
        // Stored as the parser normalizes it (e.g. without line breaks), so it's always a valid
        // Location header.
        let Ok(url) = url::Url::parse(&decode_attr(raw)).map(String::from) else {
            continue;
        };
        sqlx::query("INSERT INTO links (key, campaign_id, url) VALUES ($1, $2, $3) ON CONFLICT (campaign_id, url) DO NOTHING")
            .bind(crate::db::random_token(12))
            .bind(campaign_id)
            .bind(&url)
            .execute(db)
            .await?;
        let (key,): (String,) =
            sqlx::query_as("SELECT key FROM links WHERE campaign_id = $1 AND url = $2")
                .bind(campaign_id)
                .bind(&url)
                .fetch_one(db)
                .await?;
        out.insert(raw.to_string(), key);
        if out.len() >= 200 {
            break;
        }
    }
    Ok(out)
}

/// Rewrites tracked links in the rendered HTML and adds the open pixel.
pub fn apply(
    email: &mut OutgoingEmail,
    base: &str,
    campaign_id: i64,
    token: &str,
    links: &HashMap<String, String>,
) {
    let token = urlencode(token);
    if !links.is_empty() {
        email.html = href_re()
            .replace_all(&email.html, |cap: &regex_lite::Captures| {
                let raw = cap
                    .get(3)
                    .or(cap.get(4))
                    .or(cap.get(5))
                    .map_or("", |m| m.as_str());
                match links.get(raw) {
                    Some(key) => format!("{}\"{base}/c/{key}/{token}\"", &cap[1]),
                    None => cap[0].to_string(),
                }
            })
            .into_owned();
    }
    let pixel = format!(
        r#"<img src="{base}/o/{campaign_id}/{token}" width="1" height="1" alt="" style="display:block;width:1px;height:1px;border:0">"#
    );
    match email.html.rfind("</body>") {
        Some(i) => email.html.insert_str(i, &pixel),
        None => email.html.push_str(&pixel),
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// GET /c/{key}/{token}: records a click and redirects to the link.
pub async fn click(
    State(st): State<AppState>,
    Path((key, token)): Path<(String, String)>,
) -> Response {
    let row: Result<Option<(String,)>, _> = sqlx::query_as("SELECT url FROM links WHERE key = $1")
        .bind(&key)
        .fetch_optional(&st.db)
        .await;
    let Ok(Some((url,))) = row else {
        return (StatusCode::NOT_FOUND, "Link not found").into_response();
    };
    // Recorded only for a subscription of the campaign's own newsletter (tests and previews aren't).
    let recorded = sqlx::query(
        "INSERT INTO engagements (campaign_id, subscription_id, kind, link_id, created_at) \
         SELECT l.campaign_id, s.id, 'click', l.id, $3 FROM links l JOIN campaigns c ON c.id = l.campaign_id \
         JOIN subscriptions s ON s.list_id = c.list_id AND s.token = $2 WHERE l.key = $1",
    )
    .bind(&key)
    .bind(&token)
    .bind(now())
    .execute(&st.db)
    .await;
    if let Err(e) = recorded {
        tracing::warn!("recording a click: {e}");
    }
    Redirect::to(&url).into_response()
}

const PIXEL_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// GET /o/{campaign_id}/{token}: records the first open and returns a transparent pixel.
pub async fn open(
    State(st): State<AppState>,
    Path((campaign_id, token)): Path<(i64, String)>,
) -> Response {
    let recorded = sqlx::query(
        "INSERT INTO engagements (campaign_id, subscription_id, kind, created_at) \
         SELECT c.id, s.id, 'open', $3 FROM campaigns c JOIN subscriptions s ON s.list_id = c.list_id AND s.token = $2 \
         WHERE c.id = $1 AND NOT EXISTS (SELECT 1 FROM engagements e WHERE e.campaign_id = c.id AND e.subscription_id = s.id AND e.kind = 'open')",
    )
    .bind(campaign_id)
    .bind(&token)
    .bind(now())
    .execute(&st.db)
    .await;
    if let Err(e) = recorded {
        tracing::warn!("recording open of campaign {campaign_id}: {e}");
    }
    (
        [
            (header::CONTENT_TYPE, "image/gif"),
            (header::CACHE_CONTROL, "no-store, max-age=0"),
        ],
        PIXEL_GIF,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_only_registered_links() {
        let mut e = OutgoingEmail {
            to_email: String::new(),
            to_name: String::new(),
            subject: String::new(),
            html: r#"<body><a href="https://x.com/?a=1&amp;b=2">x</a> <a href='https://y.com'>y</a> <a href="https://n.example/unsubscribe/t">u</a></body>"#.into(),
            text: String::new(),
            unsubscribe_url: None,
            list_id: None,
        };
        let links = HashMap::from([
            ("https://x.com/?a=1&amp;b=2".to_string(), "k7".to_string()),
            ("https://y.com".to_string(), "k8".to_string()),
        ]);
        apply(&mut e, "https://news.example.com", 3, "tok+/", &links);
        assert!(
            e.html
                .contains(r#"href="https://news.example.com/c/k7/tok%2B%2F""#),
            "{}",
            e.html
        );
        assert!(e
            .html
            .contains(r#"href="https://news.example.com/c/k8/tok%2B%2F""#));
        assert!(
            e.html.contains(r#"href="https://n.example/unsubscribe/t""#),
            "untracked link kept"
        );
        assert!(e
            .html
            .contains(r#"<img src="https://news.example.com/o/3/tok%2B%2F""#));
        assert!(e.html.ends_with("</body>"));
        let mut e2 = OutgoingEmail {
            html: "<a href=https://z.com/buy>z</a>".into(),
            ..e
        };
        apply(
            &mut e2,
            "https://n",
            3,
            "t",
            &HashMap::from([("https://z.com/buy".to_string(), "k9".to_string())]),
        );
        assert!(
            e2.html.starts_with(r#"<a href="https://n/c/k9/t">z</a>"#),
            "{}",
            e2.html
        );
        assert_eq!(
            decode_attr("https://x.com/?a=1&amp;b=2"),
            "https://x.com/?a=1&b=2"
        );
        assert_eq!(
            decode_attr("https://x.com/?a=1&#x26;b=2&#38;c=3"),
            "https://x.com/?a=1&b=2&c=3"
        );
        assert_eq!(
            decode_attr("https://x.com/?q=a&b&bogus;"),
            "https://x.com/?q=a&b&bogus;"
        );
        let u = url::Url::parse(&decode_attr("https://x.com/a\n b&#10;c"))
            .map(String::from)
            .unwrap();
        assert!(axum::http::HeaderValue::from_str(&u).is_ok(), "{u}");
    }
}
