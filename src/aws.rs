//! Minimal AWS client: SigV4 signing plus the few SES v2 / SNS / IAM / STS calls Listaculous
//! needs. Avoids the AWS SDK (large compile and binary) for a handful of HTTPS requests.

use anyhow::{anyhow, bail};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct Creds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// RFC 3986 encoding as SigV4 requires (unreserved characters pass through).
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Returns the headers to add (`x-amz-date`, `authorization`, optional security token) for a
/// request. `headers` are the other headers to sign (must include `host`); names lowercase.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    creds: &Creds,
    service: &str,
    region: &str,
    method: &str,
    path: &str,
    query: &[(String, String)],
    headers: &[(&str, String)],
    body: &[u8],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<(String, String)> {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();

    let mut all: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_lowercase(), v.trim().to_string())).collect();
    all.push(("x-amz-date".into(), amz_date.clone()));
    if let Some(tok) = &creds.session_token {
        all.push(("x-amz-security-token".into(), tok.clone()));
    }
    all.sort();
    let signed_headers = all.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical_headers: String = all.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();

    let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, true), uri_encode(v, true))).collect();
    q.sort();
    let canonical_query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    // Non-S3 services double-encode path segments; ours are already plain (encoded once by the caller).
    let canonical_path = uri_encode(path, false);

    let canonical_request = format!(
        "{method}\n{canonical_path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{}",
        hex::encode(Sha256::digest(body))
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let k_date = hmac(format!("AWS4{}", creds.secret_access_key).as_bytes(), &date);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    let k_signing = hmac(&k_service, "aws4_request");
    let signature = hex::encode(hmac(&k_signing, &string_to_sign));

    let mut out = vec![
        ("x-amz-date".to_string(), amz_date),
        (
            "authorization".to_string(),
            format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", creds.access_key_id),
        ),
    ];
    if let Some(tok) = &creds.session_token {
        out.push(("x-amz-security-token".to_string(), tok.clone()));
    }
    out
}

/// An AWS API error with its code (e.g. `AlreadyExistsException`, `EntityAlreadyExists`).
#[derive(Debug)]
pub struct AwsError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for AwsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AWS {} ({}): {}", self.code, self.status, self.message)
    }
}
impl std::error::Error for AwsError {}

pub struct Aws {
    pub creds: Creds,
    pub region: String,
    pub http: reqwest::Client,
}

impl Aws {
    pub fn new(creds: Creds, region: &str, http: reqwest::Client) -> anyhow::Result<Self> {
        if region.is_empty() || !region.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("invalid AWS region");
        }
        Ok(Self { creds, region: region.to_string(), http })
    }

    async fn send(
        &self,
        service: &str,
        region: &str,
        host: &str,
        method: &str,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> anyhow::Result<(u16, String)> {
        let headers = [("host", host.to_string()), ("content-type", content_type.to_string())];
        let extra = sign(&self.creds, service, region, method, path, &[], &headers, &body, chrono::Utc::now());
        let mut req = self
            .http
            .request(method.parse()?, format!("https://{host}{path}"))
            .header("content-type", content_type)
            .body(body);
        for (k, v) in extra {
            req = req.header(k, v);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        Ok((status, resp.text().await.unwrap_or_default()))
    }

    /// SES v2 REST/JSON call, e.g. `ses("POST", "/v2/email/identities", Some(body))`.
    pub async fn ses(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, AwsError> {
        let host = format!("email.{}.amazonaws.com", self.region);
        let bytes = body.map(|b| serde_json::to_vec(&b).unwrap_or_default()).unwrap_or_default();
        let (status, text) = self
            .send("ses", &self.region, &host, method, path, "application/json", bytes)
            .await
            .map_err(|e| AwsError { status: 0, code: "NetworkError".into(), message: format!("{e:#}") })?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(v);
        }
        let code = v["__type"].as_str().or_else(|| v["code"].as_str()).unwrap_or("Error");
        Err(AwsError {
            status,
            code: code.rsplit('#').next().unwrap_or(code).to_string(),
            message: v["message"].as_str().or_else(|| v["Message"].as_str()).unwrap_or(&text).to_string(),
        })
    }

    /// AWS Query API (SNS, IAM, STS): form-encoded POST, XML response.
    pub async fn query(&self, service: &str, params: &[(&str, &str)]) -> Result<String, AwsError> {
        let (host, region, version) = match service {
            "sns" => (format!("sns.{}.amazonaws.com", self.region), self.region.clone(), "2010-03-31"),
            "iam" => ("iam.amazonaws.com".to_string(), "us-east-1".to_string(), "2010-05-08"),
            "sts" => ("sts.amazonaws.com".to_string(), "us-east-1".to_string(), "2011-06-15"),
            _ => return Err(AwsError { status: 0, code: "Unsupported".into(), message: service.into() }),
        };
        let mut form: Vec<(&str, &str)> = params.to_vec();
        form.push(("Version", version));
        let body = serde_urlencoded::to_string(&form).unwrap_or_default().into_bytes();
        let (status, text) = self
            .send(service, &region, &host, "POST", "/", "application/x-www-form-urlencoded; charset=utf-8", body)
            .await
            .map_err(|e| AwsError { status: 0, code: "NetworkError".into(), message: format!("{e:#}") })?;
        if (200..300).contains(&status) {
            return Ok(text);
        }
        Err(AwsError {
            status,
            code: xml_tag(&text, "Code").unwrap_or_else(|| "Error".into()),
            message: xml_tag(&text, "Message").unwrap_or(text),
        })
    }

    /// Validates the credentials; returns (account id, caller ARN).
    pub async fn caller_identity(&self) -> anyhow::Result<(String, String)> {
        let xml = self.query("sts", &[("Action", "GetCallerIdentity")]).await?;
        Ok((xml_tag(&xml, "Account").ok_or_else(|| anyhow!("no account in response"))?, xml_tag(&xml, "Arn").unwrap_or_default()))
    }
}

/// Extracts the text of the first `<tag>…</tag>` (good enough for AWS query responses).
pub fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(
        xml[start..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-answer test from the AWS SigV4 test suite ("get-vanilla").
    #[test]
    fn sigv4_matches_aws_test_suite() {
        let creds = Creds {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let now = chrono::DateTime::parse_from_rfc3339("2015-08-30T12:36:00Z").unwrap().with_timezone(&chrono::Utc);
        let out = sign(&creds, "service", "us-east-1", "GET", "/", &[], &[("host", "example.amazonaws.com".into())], b"", now);
        let auth = &out.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// "get-vanilla-query-order-key-case": query parameters are sorted and encoded.
    #[test]
    fn sigv4_sorts_query() {
        let creds = Creds {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let now = chrono::DateTime::parse_from_rfc3339("2015-08-30T12:36:00Z").unwrap().with_timezone(&chrono::Utc);
        let q = vec![("Param2".to_string(), "value2".to_string()), ("Param1".to_string(), "value1".to_string())];
        let out = sign(&creds, "service", "us-east-1", "GET", "/", &q, &[("host", "example.amazonaws.com".into())], b"", now);
        assert!(out[1].1.ends_with("Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"), "{}", out[1].1);
    }

    #[test]
    fn parses_query_xml() {
        let xml = "<GetCallerIdentityResponse><GetCallerIdentityResult><Arn>arn:aws:iam::123:user/x</Arn><Account>123</Account></GetCallerIdentityResult></GetCallerIdentityResponse>";
        assert_eq!(xml_tag(xml, "Account").as_deref(), Some("123"));
        assert_eq!(xml_tag("<Message>a &amp; b</Message>", "Message").as_deref(), Some("a & b"));
    }
}
