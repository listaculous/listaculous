//! Email providers and message construction.
//!
//! Every provider receives a fully-built RFC 5322 message so headers that matter for
//! deliverability (List-Unsubscribe, List-Unsubscribe-Post, List-Id, Message-ID) are
//! identical no matter which service sends it.

use anyhow::{bail, Context};
use async_trait::async_trait;
use lettre::message::header::{HeaderName, HeaderValue};
use lettre::message::{Mailbox, MultiPart};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::Deserialize;

use crate::models::Site;

pub struct OutgoingEmail {
    pub to_email: String,
    pub to_name: String,
    pub subject: String,
    pub html: String,
    pub text: String,
    /// One-click unsubscribe URL; when set, bulk headers are added.
    pub unsubscribe_url: Option<String>,
    pub list_id: Option<String>,
}

/// How a failed send should be treated. Getting this right is what prevents duplicate emails:
/// only failures where the provider definitely did *not* accept the message may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Provider definitely rejected it, but trying again later may work (throttling, 4xx SMTP).
    Retryable,
    /// Provider definitely rejected it and retrying won't help (bad config, invalid address).
    Permanent,
    /// We can't tell whether it was accepted (timeout, connection dropped). Never retried.
    Unknown,
}

#[derive(Debug)]
pub struct SendError {
    pub kind: FailureKind,
    pub message: String,
}

impl SendError {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SendError {}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Sends a raw message and returns the provider's message id.
    async fn send(&self, message: &Message) -> Result<String, SendError>;
}

pub fn provider_for(site: &Site, http: &reqwest::Client) -> anyhow::Result<Box<dyn Provider>> {
    match site.provider_kind.as_str() {
        "ses" => {
            let cfg: SesConfig =
                serde_json::from_value(crate::crypto::open_config(&site.provider_config)?)
                    .context("invalid SES config")?;
            Ok(Box::new(SesProvider::new(cfg, http.clone())?))
        }
        "smtp" => {
            let cfg: SmtpConfig =
                serde_json::from_value(crate::crypto::open_config(&site.provider_config)?)
                    .context("invalid SMTP config")?;
            Ok(Box::new(SmtpProvider::new(cfg)?))
        }
        "log" => Ok(Box::new(LogProvider)),
        other => bail!("unknown provider '{other}'"),
    }
}

pub fn build_message(site: &Site, email: &OutgoingEmail) -> anyhow::Result<Message> {
    let from: Mailbox = Mailbox::new(
        Some(site.from_name.clone()).filter(|s| !s.is_empty()),
        site.from_email.parse().context("invalid site from_email")?,
    );
    let to = Mailbox::new(
        Some(email.to_name.clone()).filter(|s| !s.is_empty()),
        email
            .to_email
            .parse()
            .context("invalid recipient address")?,
    );
    let domain = site.from_email.rsplit('@').next().unwrap_or("localhost");
    let mut builder = Message::builder()
        .from(from)
        .to(to)
        .subject(email.subject.clone())
        .message_id(Some(format!(
            "<{}@{}>",
            crate::db::random_token(18),
            domain
        )));
    if !site.reply_to.is_empty() {
        builder = builder.reply_to(site.reply_to.parse().context("invalid reply_to")?);
    }
    if let Some(url) = &email.unsubscribe_url {
        // RFC 2369 + RFC 8058 one-click unsubscribe (required by Gmail/Yahoo for bulk senders).
        builder = builder
            .raw_header(HeaderValue::new(
                HeaderName::new_from_ascii_str("List-Unsubscribe"),
                format!("<{url}>"),
            ))
            .raw_header(HeaderValue::new(
                HeaderName::new_from_ascii_str("List-Unsubscribe-Post"),
                "List-Unsubscribe=One-Click".into(),
            ))
            .raw_header(HeaderValue::new(
                HeaderName::new_from_ascii_str("Precedence"),
                "bulk".into(),
            ));
    }
    if let Some(list_id) = &email.list_id {
        builder = builder.raw_header(HeaderValue::new(
            HeaderName::new_from_ascii_str("List-Id"),
            list_id.clone(),
        ));
    }
    Ok(builder.multipart(MultiPart::alternative_plain_html(
        email.text.clone(),
        email.html.clone(),
    ))?)
}

// ---------------------------------------------------------------------------------------
// Log provider: prints emails instead of sending. Useful for local testing.

pub struct LogProvider;

#[async_trait]
impl Provider for LogProvider {
    async fn send(&self, message: &Message) -> Result<String, SendError> {
        let raw = String::from_utf8_lossy(&message.formatted()).to_string();
        tracing::info!("[log provider] email:\n{raw}");
        Ok(format!("log-{}", crate::db::random_token(8)))
    }
}

// ---------------------------------------------------------------------------------------
// SMTP provider (works with Postmark, Mailgun, SendGrid, Brevo, SES SMTP, etc.)

#[derive(Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// "starttls" (default), "tls", or "none"
    #[serde(default)]
    pub security: String,
}

pub struct SmtpProvider {
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl SmtpProvider {
    pub fn new(cfg: SmtpConfig) -> anyhow::Result<Self> {
        use lettre::transport::smtp::authentication::Credentials;
        let mut builder = match cfg.security.as_str() {
            "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)?,
            "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host),
            _ => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)?,
        };
        if let Some(port) = cfg.port {
            builder = builder.port(port);
        }
        // Per-command timeout: a server that accepts the connection but stops responding must not
        // hang the sender forever.
        builder = builder.timeout(Some(std::time::Duration::from_secs(60)));
        if !cfg.username.is_empty() {
            builder = builder.credentials(Credentials::new(cfg.username, cfg.password));
        }
        Ok(Self {
            transport: builder.build(),
        })
    }
}

#[async_trait]
impl Provider for SmtpProvider {
    async fn send(&self, message: &Message) -> Result<String, SendError> {
        // Overall deadline on top of the per-command timeout; expiry leaves delivery uncertain.
        let sent = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.transport.send(message.clone()),
        )
        .await;
        let Ok(sent) = sent else {
            return Err(SendError::new(FailureKind::Unknown, "SMTP: send timed out"));
        };
        match sent {
            Ok(resp) => Ok(resp.message().collect::<Vec<_>>().join(" ")),
            Err(e) => {
                // An SMTP reply code means the server answered definitively; anything else
                // (I/O error, dropped connection, TLS failure) leaves delivery uncertain.
                // Never reaching the server (refused/unreachable) may be a temporary outage: retry.
                let kind = if e.is_transient() || e.is_tls() || never_connected(&e) {
                    FailureKind::Retryable
                } else if e.is_permanent() || e.is_client() {
                    FailureKind::Permanent
                } else {
                    FailureKind::Unknown
                };
                Err(SendError::new(kind, format!("SMTP: {e}")))
            }
        }
    }
}

/// True when the error chain shows we never reached the server (so nothing was delivered).
fn never_connected(e: &(dyn std::error::Error + 'static)) -> bool {
    use std::io::ErrorKind::*;
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                ConnectionRefused
                    | NotFound
                    | AddrNotAvailable
                    | HostUnreachable
                    | NetworkUnreachable
            );
        }
        cur = err.source();
    }
    false
}

// ---------------------------------------------------------------------------------------
// Amazon SES v2 via the HTTPS API with a minimal SigV4 signer (avoids the heavy AWS SDK).

#[derive(Deserialize)]
pub struct SesConfig {
    pub region: String,
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    #[serde(default)]
    pub configuration_set: String,
}

pub struct SesProvider {
    cfg: SesConfig,
    session_token: Option<String>,
    http: reqwest::Client,
}

impl SesProvider {
    pub fn new(mut cfg: SesConfig, http: reqwest::Client) -> anyhow::Result<Self> {
        let mut session_token = None;
        if cfg.access_key_id.is_empty() {
            // Fall back to the standard AWS environment variables.
            cfg.access_key_id = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default();
            cfg.secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
            session_token = std::env::var("AWS_SESSION_TOKEN")
                .ok()
                .filter(|s| !s.is_empty());
        }
        if cfg.access_key_id.is_empty() || cfg.secret_access_key.is_empty() {
            bail!("SES credentials missing (set them on the site or via AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY)");
        }
        if cfg.region.is_empty()
            || !cfg
                .region
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            bail!("invalid SES region");
        }
        Ok(Self {
            cfg,
            session_token,
            http,
        })
    }
}

#[async_trait]
impl Provider for SesProvider {
    async fn send(&self, message: &Message) -> Result<String, SendError> {
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD.encode(message.formatted());
        let mut body = serde_json::json!({ "Content": { "Raw": { "Data": raw } } });
        if !self.cfg.configuration_set.is_empty() {
            body["ConfigurationSetName"] = self.cfg.configuration_set.clone().into();
        }
        let body = serde_json::to_vec(&body)
            .map_err(|e| SendError::new(FailureKind::Permanent, e.to_string()))?;

        let host = format!("email.{}.amazonaws.com", self.cfg.region);
        let path = "/v2/email/outbound-emails";
        let creds = crate::aws::Creds {
            access_key_id: self.cfg.access_key_id.clone(),
            secret_access_key: self.cfg.secret_access_key.clone(),
            session_token: self.session_token.clone(),
        };
        let signed = crate::aws::sign(
            &creds,
            "ses",
            &self.cfg.region,
            "POST",
            path,
            &[],
            &[
                ("host", host.clone()),
                ("content-type", "application/json".into()),
            ],
            &body,
            chrono::Utc::now(),
        );
        let mut req = self
            .http
            .post(format!("https://{host}{path}"))
            .header("content-type", "application/json")
            .body(body);
        for (k, v) in signed {
            req = req.header(k, v);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            // Failed to connect at all: the request never reached SES, so retrying is safe.
            Err(e) if e.is_connect() => {
                return Err(SendError::new(
                    FailureKind::Retryable,
                    format!("SES connect: {e}"),
                ))
            }
            Err(e) => {
                return Err(SendError::new(
                    FailureKind::Unknown,
                    format!("SES request: {e}"),
                ))
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let kind =
                if status.as_u16() == 429 || status.is_server_error() && status.as_u16() != 504 {
                    FailureKind::Retryable
                } else if status.as_u16() == 504 {
                    FailureKind::Unknown
                } else {
                    FailureKind::Permanent
                };
            return Err(SendError::new(kind, format!("SES error {status}: {text}")));
        }
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        Ok(v["MessageId"].as_str().unwrap_or_default().to_string())
    }
}

// ---------------------------------------------------------------------------------------
// Content helpers

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Very small HTML → plain text converter for the text/plain alternative part.
pub fn html_to_text(html: &str) -> String {
    use regex_lite::Regex;
    let s = Regex::new(r"(?is)<(script|style|head)[^>]*>.*?</(script|style|head)>")
        .unwrap()
        .replace_all(html, "");
    let s = Regex::new(r#"(?is)<a\s[^>]*href\s*=\s*["']([^"']+)["'][^>]*>(.*?)</a>"#)
        .unwrap()
        .replace_all(&s, "$2 ($1)");
    let s = Regex::new(r"(?i)<br\s*/?>").unwrap().replace_all(&s, "\n");
    let s = Regex::new(r"(?i)</(p|div|h[1-6]|li|tr|table|blockquote)>")
        .unwrap()
        .replace_all(&s, "\n\n");
    let s = Regex::new(r"(?i)<li[^>]*>").unwrap().replace_all(&s, "• ");
    let s = Regex::new(r"(?s)<[^>]+>").unwrap().replace_all(&s, "");
    let s = s
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let lines: Vec<&str> = s.lines().map(|l| l.trim()).collect();
    let mut out = String::new();
    let mut blank = 0;
    for l in lines {
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim().to_string()
}
