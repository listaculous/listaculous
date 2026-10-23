//! Connected cloud accounts (Amazon Web Services, Porkbun) used for automatic sending setup.
//! Secrets are encrypted at rest and never returned by the API.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthUser;
use crate::aws::{Aws, Creds};
use crate::db::now;
use crate::dns::{DnsProvider, Porkbun};
use crate::error::{AppError, AppResult};
use crate::AppState;

pub struct Integration {
    pub kind: String,
    pub name: String,
    /// Decrypted config (only ever held in memory).
    pub config: Value,
}

impl Integration {
    pub fn aws(&self, http: &reqwest::Client, region: Option<&str>) -> AppResult<Aws> {
        if self.kind != "aws" {
            return Err(AppError::bad_request(
                "that integration isn't an AWS account",
            ));
        }
        let s = |k: &str| self.config[k].as_str().unwrap_or_default().to_string();
        let region = region
            .filter(|r| !r.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| s("region"));
        Aws::new(
            Creds {
                access_key_id: s("access_key_id"),
                secret_access_key: s("secret_access_key"),
                session_token: None,
            },
            &region,
            http.clone(),
        )
        .map_err(|e| AppError::bad_request(format!("{e:#}")))
    }

    pub fn dns(&self, http: &reqwest::Client) -> AppResult<Box<dyn DnsProvider>> {
        match self.kind.as_str() {
            "porkbun" => {
                let s = |k: &str| self.config[k].as_str().unwrap_or_default().to_string();
                Ok(Box::new(Porkbun::new(
                    s("api_key"),
                    s("secret_api_key"),
                    http.clone(),
                )))
            }
            _ => Err(AppError::bad_request(
                "that integration isn't a DNS provider",
            )),
        }
    }
}

pub async fn load(st: &AppState, user: &AuthUser, id: i64) -> AppResult<Integration> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT kind, name, config FROM integrations WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(user.account_id)
    .fetch_optional(&st.db)
    .await?;
    let (kind, name, config) = row.ok_or_else(AppError::not_found)?;
    Ok(Integration {
        kind,
        name,
        config: crate::crypto::open_config(&config)?,
    })
}

fn public_json(
    id: i64,
    kind: &str,
    name: &str,
    config: &Value,
    details: &Value,
    created_at: &str,
) -> Value {
    // Show only non-secret identifying bits.
    let hint = |k: &str| {
        config[k].as_str().map(|s| {
            if s.len() > 8 {
                format!("{}…", &s[..s.len().min(8)])
            } else {
                "set".into()
            }
        })
    };
    let summary = match kind {
        "aws" => json!({ "access_key_id": hint("access_key_id"), "region": config["region"] }),
        "porkbun" => json!({ "api_key": hint("api_key") }),
        _ => json!({}),
    };
    json!({ "id": id, "kind": kind, "name": name, "summary": summary, "details": details, "created_at": created_at })
}

pub async fn list(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let rows: Vec<(i64, String, String, String, String, String)> = sqlx::query_as(
        "SELECT id, kind, name, config, details, created_at FROM integrations WHERE account_id = $1 ORDER BY id",
    )
    .bind(user.account_id)
    .fetch_all(&st.db)
    .await?;
    let mut out = Vec::new();
    for (id, kind, name, config, details, created_at) in rows {
        let cfg = crate::crypto::open_config(&config).unwrap_or_default();
        let details: Value = serde_json::from_str(&details).unwrap_or_default();
        out.push(public_json(id, &kind, &name, &cfg, &details, &created_at));
    }
    Ok(Json(Value::Array(out)))
}

#[derive(Deserialize)]
pub struct CreateIntegration {
    pub kind: String,
    #[serde(default)]
    pub name: String,
    pub config: Value,
}

/// Verifies the credentials against the provider, then stores them (encrypted).
pub async fn create(
    State(st): State<AppState>,
    user: AuthUser,
    Json(c): Json<CreateIntegration>,
) -> AppResult<(StatusCode, Json<Value>)> {
    if user.scope == crate::api_keys::SCOPE_AWS_SETUP && c.kind != "aws" {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "this API key can only connect an AWS account",
        ));
    }
    let str_field = |k: &str| c.config[k].as_str().unwrap_or_default().trim().to_string();
    let (config, details, default_name) = match c.kind.as_str() {
        "aws" => {
            let config = json!({
                "access_key_id": str_field("access_key_id"),
                "secret_access_key": str_field("secret_access_key"),
                "region": if str_field("region").is_empty() { "us-east-1".to_string() } else { str_field("region") },
            });
            if config["access_key_id"] == "" || config["secret_access_key"] == "" {
                return Err(AppError::bad_request(
                    "access_key_id and secret_access_key are required",
                ));
            }
            let tmp = Integration {
                kind: "aws".into(),
                name: String::new(),
                config: config.clone(),
            };
            let aws = tmp.aws(&st.http, None)?;
            let (account, arn) = aws.caller_identity().await.map_err(|e| {
                AppError::bad_request(format!("AWS rejected these credentials: {e:#}"))
            })?;
            (
                config,
                json!({ "aws_account_id": account, "arn": arn }),
                format!("AWS {account}"),
            )
        }
        "porkbun" => {
            let config = json!({ "api_key": str_field("api_key"), "secret_api_key": str_field("secret_api_key") });
            if config["api_key"] == "" || config["secret_api_key"] == "" {
                return Err(AppError::bad_request(
                    "api_key and secret_api_key are required",
                ));
            }
            Porkbun::new(
                str_field("api_key"),
                str_field("secret_api_key"),
                st.http.clone(),
            )
            .ping()
            .await
            .map_err(|e| AppError::bad_request(format!("Porkbun rejected these keys: {e:#}")))?;
            (config, json!({}), "Porkbun".to_string())
        }
        _ => return Err(AppError::bad_request("kind must be 'aws' or 'porkbun'")),
    };
    let name = if c.name.trim().is_empty() {
        default_name
    } else {
        c.name.trim().chars().take(60).collect()
    };
    let mut sealed = config.clone();
    crate::crypto::seal_config(&mut sealed)?;
    let created_at = now();
    let mut details = details;
    // A setup key is recorded on the integration so the page can tell that *its* attempt connected.
    let setup_key = (user.scope == crate::api_keys::SCOPE_AWS_SETUP)
        .then_some(user.api_key_id)
        .flatten();
    if let Some(k) = setup_key {
        details["setup_key_id"] = k.into();
    }
    let id = insert(
        &st.db,
        user.account_id,
        &c.kind,
        &name,
        &sealed,
        &details,
        &created_at,
        setup_key,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(public_json(
            id,
            &c.kind,
            &name,
            &config,
            &details,
            &created_at,
        )),
    ))
}

/// Inserts an integration. With a single-use setup key, the key is consumed in the same
/// transaction: it must still exist (not used by a concurrent request, not revoked/cancelled
/// meanwhile, not expired), otherwise nothing is inserted.
#[allow(clippy::too_many_arguments)]
async fn insert(
    db: &crate::db::Db,
    account_id: i64,
    kind: &str,
    name: &str,
    sealed: &Value,
    details: &Value,
    created_at: &str,
    setup_key: Option<i64>,
) -> AppResult<i64> {
    let mut tx = db.begin().await?;
    if let Some(key) = setup_key {
        let r = sqlx::query("DELETE FROM api_keys WHERE id = $1 AND scope = 'aws_setup' AND (expires_at IS NULL OR expires_at > $2)")
            .bind(key)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        if r.rows_affected() != 1 {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                "this setup key was already used, revoked or expired; get a new setup command",
            ));
        }
    }
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO integrations (account_id, kind, name, config, details, created_at) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(account_id)
    .bind(kind)
    .bind(name)
    .bind(sealed.to_string())
    .bind(details.to_string())
    .bind(created_at)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn remove(
    State(st): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let r = sqlx::query("DELETE FROM integrations WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(user.account_id)
        .execute(&st.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::not_found());
    }
    Ok(Json(json!({ "ok": true })))
}

/// The IAM policy to attach to the *setup* user. Send-only users that Listaculous creates live
/// under the `/listaculous/` IAM path, which keeps the IAM permissions scoped to them.
pub fn setup_policy() -> Value {
    json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Sid": "ListaculousSesSetup",
                "Effect": "Allow",
                "Action": [
                    "ses:GetAccount", "ses:PutAccountDetails",
                    "ses:CreateEmailIdentity", "ses:GetEmailIdentity",
                    "ses:PutEmailIdentityMailFromAttributes", "ses:PutEmailIdentityConfigurationSetAttributes",
                    "ses:CreateConfigurationSet", "ses:GetConfigurationSet",
                    "ses:CreateConfigurationSetEventDestination", "ses:GetConfigurationSetEventDestinations",
                    "ses:UpdateConfigurationSetEventDestination",
                    "ses:PutEmailIdentityDkimAttributes", "ses:PutEmailIdentityDkimSigningAttributes",
                    "ses:TagResource"
                ],
                "Resource": "*"
            },
            {
                "Sid": "ListaculousFeedbackTopics",
                "Effect": "Allow",
                "Action": ["sns:CreateTopic", "sns:GetTopicAttributes", "sns:SetTopicAttributes", "sns:Subscribe", "sns:ListSubscriptionsByTopic"],
                "Resource": "arn:aws:sns:*:*:listaculous-*"
            },
            {
                "Sid": "ListaculousSendOnlyUsers",
                "Effect": "Allow",
                "Action": ["iam:CreateUser", "iam:GetUser", "iam:PutUserPolicy", "iam:CreateAccessKey", "iam:ListAccessKeys", "iam:TagUser"],
                "Resource": "arn:aws:iam::*:user/listaculous/*"
            }
        ]
    })
}

pub async fn aws_policy(_user: AuthUser) -> Json<Value> {
    Json(setup_policy())
}

const AWS_SETUP_SCRIPT: &str = include_str!("scripts/aws-setup.sh");

/// A bash script that creates the setup IAM user with the AWS CLI (policy embedded, so it always
/// matches `setup_policy`), optionally connecting it to this server via an API key.
pub fn aws_setup_script(base_url: &str, region: &str) -> String {
    let region = if region
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !region.is_empty()
    {
        region
    } else {
        "us-east-1"
    };
    AWS_SETUP_SCRIPT
        .replace(
            "__POLICY__",
            &serde_json::to_string_pretty(&setup_policy()).unwrap_or_default(),
        )
        .replace("__URL__", base_url)
        .replace("__REGION__", region)
}

#[derive(Deserialize, Default)]
pub struct ScriptQuery {
    #[serde(default)]
    pub region: String,
}

pub async fn aws_script(
    State(st): State<AppState>,
    _user: AuthUser,
    axum::extract::Query(q): axum::extract::Query<ScriptQuery>,
) -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/x-shellscript; charset=utf-8",
        )],
        aws_setup_script(&st.cfg.base_url, &q.region),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// A setup key connects at most one account: a second (concurrent) use, or a use after the
    /// key was revoked mid-verification, inserts nothing.
    #[tokio::test]
    async fn setup_key_is_consumed_exactly_once() {
        let path = std::env::temp_dir().join(format!("lc-int-{}.db", crate::db::random_token(8)));
        let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query("INSERT INTO accounts (name, created_at) VALUES ('a', 'now')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO users (account_id, email, password_hash, created_at) VALUES (1, 'a@b.c', 'x', 'now')").execute(&db).await.unwrap();
        for (id, scope) in [(1, "aws_setup"), (2, "aws_setup")] {
            sqlx::query("INSERT INTO api_keys (id, user_id, name, prefix, token_hash, created_at, scope) VALUES ($1, 1, 'k', 'p', $2, 'now', $3)")
                .bind(id as i64).bind(format!("h{id}")).bind(scope).execute(&db).await.unwrap();
        }
        let cfg = json!({});
        let first = super::insert(&db, 1, "aws", "A", &cfg, &cfg, "now", Some(1));
        let second = super::insert(&db, 1, "aws", "B", &cfg, &cfg, "now", Some(1));
        let (a, b) = tokio::join!(first, second);
        assert_eq!(
            [a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(),
            1,
            "exactly one concurrent use wins"
        );

        // Revoked (e.g. cancelled) while the AWS check was running: nothing inserted.
        sqlx::query("DELETE FROM api_keys WHERE id = 2")
            .execute(&db)
            .await
            .unwrap();
        assert!(
            super::insert(&db, 1, "aws", "C", &cfg, &cfg, "now", Some(2))
                .await
                .is_err()
        );
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM integrations")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn script_embeds_current_policy_and_url() {
        let s = super::aws_setup_script("https://lc.example.com", "eu-west-1");
        assert!(!s.contains("__POLICY__") && !s.contains("__URL__") && !s.contains("__REGION__"));
        assert!(s.contains(r#"LISTACULOUS_URL="https://lc.example.com""#));
        assert!(
            s.contains("ses:CreateEmailIdentity")
                && s.contains("arn:aws:iam::*:user/listaculous/*")
        );
        assert!(s.contains("${LISTACULOUS_SES_REGION:-eu-west-1}"));
        // An unsafe region can't inject into the script.
        assert!(super::aws_setup_script("https://x", "a\"; rm -rf /").contains(":-us-east-1}"));
    }
}
