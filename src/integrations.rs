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
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO integrations (account_id, kind, name, config, details, created_at) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(user.account_id)
    .bind(&c.kind)
    .bind(&name)
    .bind(sealed.to_string())
    .bind(details.to_string())
    .bind(&created_at)
    .fetch_one(&st.db)
    .await?;
    // A single-use setup key is done once it has connected an account.
    crate::api_keys::consume(&st, &user).await?;
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
