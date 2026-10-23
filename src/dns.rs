//! DNS providers used by automatic sending setup. Names are relative to the zone ("" = apex,
//! "bounce.news" = bounce.news.example.com) so plans read naturally and providers convert.

use anyhow::{bail, Context};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    /// Provider's record id (empty for records we haven't created yet).
    #[serde(default)]
    pub id: String,
    /// Relative to the zone; "" is the apex.
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub priority: Option<u16>,
    #[serde(default)]
    pub ttl: Option<u32>,
}

impl Record {
    pub fn fqdn(&self, zone: &str) -> String {
        if self.name.is_empty() {
            zone.to_string()
        } else {
            format!("{}.{zone}", self.name)
        }
    }
}

/// TXT values compare without surrounding quotes; hostnames without a trailing dot, case-insensitively.
pub fn same_content(kind: &str, a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim().trim_matches('"');
        if kind == "TXT" {
            s.to_string()
        } else {
            s.trim_end_matches('.').to_ascii_lowercase()
        }
    };
    norm(a) == norm(b)
}

#[async_trait]
pub trait DnsProvider: Send + Sync {
    async fn list(&self, zone: &str) -> anyhow::Result<Vec<Record>>;
    /// Creates a record and returns its provider id.
    async fn create(&self, zone: &str, record: &Record) -> anyhow::Result<String>;
}

// ---------------------------------------------------------------------------------------
// Porkbun (https://porkbun.com/api/json/v3/documentation). API access must be enabled per domain.

pub struct Porkbun {
    api_key: String,
    secret_api_key: String,
    http: reqwest::Client,
    base: String,
}

impl Porkbun {
    pub fn new(api_key: String, secret_api_key: String, http: reqwest::Client) -> Self {
        let mut base = "https://api.porkbun.com/api/json/v3".to_string();
        // Debug builds only: point at a mock for end-to-end tests.
        if cfg!(debug_assertions) {
            if let Ok(b) = std::env::var("LISTACULOUS_TEST_PORKBUN_ENDPOINT") {
                base = b;
            }
        }
        Self {
            api_key,
            secret_api_key,
            http,
            base,
        }
    }

    async fn call(&self, path: &str, extra: Value) -> anyhow::Result<Value> {
        let mut body = json!({ "apikey": self.api_key, "secretapikey": self.secret_api_key });
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .context("contacting Porkbun")?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if v["status"] == "SUCCESS" {
            return Ok(v);
        }
        let msg = v["message"].as_str().unwrap_or("unknown error");
        bail!("Porkbun {status}: {msg}")
    }

    /// Checks the API keys (Porkbun's /ping).
    pub async fn ping(&self) -> anyhow::Result<()> {
        self.call("/ping", json!({})).await.map(|_| ())
    }

    /// Porkbun takes a domain path segment; reject anything that isn't a plain domain.
    fn zone_path(zone: &str) -> anyhow::Result<&str> {
        if zone.is_empty()
            || !zone
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            bail!("invalid domain {zone:?}");
        }
        Ok(zone)
    }
}

#[async_trait]
impl DnsProvider for Porkbun {
    async fn list(&self, zone: &str) -> anyhow::Result<Vec<Record>> {
        let v = self
            .call(
                &format!("/dns/retrieve/{}", Self::zone_path(zone)?),
                json!({}),
            )
            .await?;
        let suffix = format!(".{zone}");
        Ok(v["records"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                let fqdn = r["name"].as_str().unwrap_or_default().to_ascii_lowercase();
                let name = if fqdn == zone {
                    String::new()
                } else {
                    fqdn.strip_suffix(&suffix).unwrap_or(&fqdn).to_string()
                };
                Record {
                    id: r["id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| r["id"].to_string()),
                    name,
                    kind: r["type"].as_str().unwrap_or_default().to_string(),
                    content: r["content"].as_str().unwrap_or_default().to_string(),
                    priority: r["prio"].as_str().and_then(|p| p.parse().ok()),
                    ttl: r["ttl"].as_str().and_then(|t| t.parse().ok()),
                }
            })
            .collect())
    }

    async fn create(&self, zone: &str, record: &Record) -> anyhow::Result<String> {
        let mut body = json!({
            "name": record.name,
            "type": record.kind,
            "content": record.content,
            "ttl": record.ttl.unwrap_or(600).to_string(),
        });
        if let Some(p) = record.priority {
            body["prio"] = p.to_string().into();
        }
        let v = self
            .call(&format!("/dns/create/{}", Self::zone_path(zone)?), body)
            .await?;
        Ok(v["id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v["id"].to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_record_content() {
        assert!(same_content(
            "CNAME",
            "abc.dkim.amazonses.com.",
            "ABC.dkim.amazonses.com"
        ));
        assert!(same_content(
            "TXT",
            "\"v=spf1 include:amazonses.com ~all\"",
            "v=spf1 include:amazonses.com ~all"
        ));
        assert!(!same_content(
            "TXT",
            "v=spf1 -all",
            "v=spf1 include:amazonses.com ~all"
        ));
        let r = Record {
            id: String::new(),
            name: "bounce.news".into(),
            kind: "MX".into(),
            content: "x".into(),
            priority: Some(10),
            ttl: None,
        };
        assert_eq!(r.fqdn("example.com"), "bounce.news.example.com");
    }

    #[test]
    fn rejects_bad_zone_paths() {
        assert!(Porkbun::zone_path("example.com").is_ok());
        assert!(Porkbun::zone_path("../ping").is_err());
        assert!(Porkbun::zone_path("").is_err());
    }
}
