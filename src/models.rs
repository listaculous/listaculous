use serde::Serialize;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Site {
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    pub domain: String,
    pub from_name: String,
    pub from_email: String,
    pub reply_to: String,
    pub physical_address: String,
    pub provider_kind: String,
    #[serde(skip)]
    pub provider_config: String,
    pub send_rate: i64,
    pub webhook_secret: String,
    pub created_at: String,
    /// MIME type of the stored favicon; None when the site has no icon.
    #[serde(skip)]
    pub icon_type: Option<String>,
    /// Last favicon fetch attempt; also used to cache-bust the icon URL.
    #[serde(skip)]
    pub icon_checked_at: Option<String>,
}

pub const SITE_COLS: &str =
    "id, account_id, name, domain, from_name, from_email, reply_to, physical_address, \
    provider_kind, provider_config, send_rate, webhook_secret, created_at, icon_type, icon_checked_at";

/// Keys inside provider_config that are secrets and never returned to the browser.
pub const SECRET_KEYS: &[&str] = &["secret_access_key", "password"];

impl Site {
    pub fn masked_provider_config(&self) -> serde_json::Value {
        let mut v: serde_json::Value =
            serde_json::from_str(&self.provider_config).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            for k in SECRET_KEYS {
                if let Some(val) = obj.get_mut(*k) {
                    if val.as_str().is_some_and(|s| !s.is_empty()) {
                        *val = "********".into();
                    }
                }
            }
        }
        v
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct List {
    pub id: i64,
    pub site_id: i64,
    pub public_id: String,
    pub name: String,
    pub description: String,
    pub double_opt_in: i64,
    pub redirect_url: String,
    pub created_at: String,
}

pub const LIST_COLS: &str =
    "id, site_id, public_id, name, description, double_opt_in, redirect_url, created_at";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Campaign {
    pub id: i64,
    pub list_id: i64,
    pub subject: String,
    pub preheader: String,
    pub html_body: String,
    pub text_body: String,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

pub const CAMPAIGN_COLS: &str =
    "id, list_id, subject, preheader, html_body, text_body, status, created_at, started_at, finished_at";

pub struct Recipient<'a> {
    pub email: &'a str,
    pub name: &'a str,
    pub token: &'a str,
}

/// Renders a campaign for one recipient, always appending the compliance footer
/// (reason for receiving, unsubscribe link, physical address — CAN-SPAM / GDPR / CASL).
pub fn render_campaign(
    base_url: &str,
    site: &Site,
    list: &List,
    c: &Campaign,
    r: &Recipient,
) -> crate::mailer::OutgoingEmail {
    use crate::mailer::{html_escape, html_to_text};
    let unsub = format!("{base_url}/unsubscribe/{}", r.token);
    let sub = |s: &str, escape: bool| {
        let e = |v: &str| {
            if escape {
                html_escape(v)
            } else {
                v.to_string()
            }
        };
        s.replace("{{unsubscribe_url}}", &unsub)
            .replace("{{email}}", &e(r.email))
            .replace("{{name}}", &e(r.name))
            .replace("{{list_name}}", &e(&list.name))
            .replace("{{site_name}}", &e(&site.name))
    };

    let body_html = sub(&c.html_body, true);
    let address_html = html_escape(&site.physical_address).replace('\n', "<br>");
    let preheader = if c.preheader.is_empty() {
        String::new()
    } else {
        format!(
            r#"<div style="display:none;max-height:0;overflow:hidden;opacity:0">{}</div>"#,
            html_escape(&c.preheader)
        )
    };
    let html = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title></head>
<body style="margin:0;padding:0;background:#f6f6f6">{preheader}
<div style="max-width:640px;margin:0 auto;padding:24px;background:#ffffff;font-family:-apple-system,Segoe UI,Helvetica,Arial,sans-serif;font-size:16px;line-height:1.5;color:#222">
{body_html}
</div>
<div style="max-width:640px;margin:0 auto;padding:16px 24px;font-family:-apple-system,Segoe UI,Helvetica,Arial,sans-serif;font-size:12px;line-height:1.5;color:#777">
You are receiving this because you subscribed to {list} from {site}.<br>
<a href="{unsub}" style="color:#777">Unsubscribe</a><br>
{address_html}
</div></body></html>"#,
        title = html_escape(&c.subject),
        list = html_escape(&list.name),
        site = html_escape(&site.name),
    );

    let body_text = if c.text_body.trim().is_empty() {
        html_to_text(&body_html)
    } else {
        sub(&c.text_body, false)
    };
    let text = format!(
        "{body_text}\n\n--\nYou are receiving this because you subscribed to {} from {}.\nUnsubscribe: {unsub}\n{}\n",
        list.name, site.name, site.physical_address
    );

    crate::mailer::OutgoingEmail {
        to_email: r.email.to_string(),
        to_name: r.name.to_string(),
        subject: sub(&c.subject, false),
        html,
        text,
        unsubscribe_url: Some(unsub),
        list_id: Some(format!(
            "{} <{}.{}>",
            list.name.replace(['<', '>', '"', '\r', '\n'], ""),
            list.public_id.to_lowercase(),
            site.domain
        )),
    }
}
