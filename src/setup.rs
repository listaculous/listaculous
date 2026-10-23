//! Automatic Amazon SES + DNS setup for a site.
//!
//! `plan` is read-only: it inspects AWS and DNS and lists every change. `apply` (explicitly
//! confirmed) performs them idempotently in the background, recording progress so it can be
//! re-run safely. It never deletes or edits DNS records; existing matching records are reused,
//! conflicting ones stop the run.
//!
//! For a site sending as `hello@news.example.com` on zone `example.com` it sets up:
//! - an SES identity `news.example.com` with Easy DKIM (3 CNAMEs),
//! - custom MAIL FROM `bounce.news.example.com` (MX + SPF TXT) for SPF alignment,
//! - a DMARC record if the domain has none (`p=none`; never replaces an existing policy),
//! - a configuration set publishing bounces/complaints to an SNS topic subscribed to the site's
//!   webhook (needs a public HTTPS BASE_URL),
//! - a send-only IAM user (under `/listaculous/`) whose key replaces the site's SES credentials.

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthUser;
use crate::aws::{xml_tag, Aws, AwsError};
use crate::db::now;
use crate::dns::{same_content, DnsProvider, Record};
use crate::error::{AppError, AppResult};
use crate::integrations::{self, Integration};
use crate::models::{Site, SITE_COLS};
use crate::AppState;

const EVENT_DESTINATION: &str = "listaculous-feedback";

#[derive(Deserialize, Default)]
pub struct SetupRequest {
    pub aws_integration_id: i64,
    /// Omit to manage DNS yourself: the plan then lists the records to add by hand.
    #[serde(default)]
    pub dns_integration_id: Option<i64>,
    /// SES region; defaults to the AWS integration's region.
    #[serde(default)]
    pub region: Option<String>,
    /// Must be true for apply.
    #[serde(default)]
    pub confirm: bool,
}

/// Everything derived from the site that the plan and apply share.
struct Target {
    site: Site,
    zone: String,
    identity: String,
    mail_from: String,
    config_set: String,
    topic: String,
    iam_user: String,
    webhook_url: String,
    webhook_public: bool,
}

fn rel(name: &str, zone: &str) -> String {
    if name == zone {
        String::new()
    } else {
        name.strip_suffix(&format!(".{zone}"))
            .unwrap_or(name)
            .to_string()
    }
}

fn target(st: &AppState, site: Site) -> AppResult<Target> {
    let zone = site.domain.trim().to_ascii_lowercase();
    let identity = site
        .from_email
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if identity != zone && !identity.ends_with(&format!(".{zone}")) {
        return Err(AppError::bad_request(format!(
            "the from email's domain ({identity}) must be {zone} or a subdomain of it (e.g. news.{zone})"
        )));
    }
    let webhook_url = format!(
        "{}/webhooks/ses/{}/{}",
        st.cfg.base_url, site.id, site.webhook_secret
    );
    let host = url::Url::parse(&st.cfg.base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let webhook_public = st.cfg.base_url.starts_with("https://")
        && !(host == "localhost"
            || host.ends_with(".localhost")
            || host.ends_with(".local")
            || host.parse::<std::net::IpAddr>().is_ok());
    Ok(Target {
        mail_from: format!("bounce.{identity}"),
        config_set: format!("listaculous-site-{}", site.id),
        topic: format!("listaculous-site-{}-feedback", site.id),
        iam_user: format!("listaculous-site-{}-sender", site.id),
        zone,
        identity,
        webhook_url,
        webhook_public,
        site,
    })
}

async fn load_site(st: &AppState, user: &AuthUser, id: i64) -> AppResult<Site> {
    sqlx::query_as(&format!(
        "SELECT {SITE_COLS} FROM sites WHERE id = $1 AND account_id = $2"
    ))
    .bind(id)
    .bind(user.account_id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(AppError::not_found)
}

fn aws_err(e: AwsError) -> AppError {
    let hint = if e.code.contains("AccessDenied") || e.status == 403 {
        " — check the setup key has the Listaculous setup policy (Integrations → AWS policy)"
    } else {
        ""
    };
    AppError::bad_request(format!("{e}{hint}"))
}

/// The records SES needs, given the DKIM tokens (if the identity exists yet).
fn wanted_records(t: &Target, region: &str, dkim_tokens: &[String]) -> Vec<(String, Record)> {
    let r = |name: &str, kind: &str, content: String, priority: Option<u16>| Record {
        id: String::new(),
        name: rel(name, &t.zone),
        kind: kind.into(),
        content,
        priority,
        ttl: Some(600),
    };
    let mut out: Vec<(String, Record)> = dkim_tokens
        .iter()
        .map(|tok| {
            (
                "dkim".to_string(),
                r(
                    &format!("{tok}._domainkey.{}", t.identity),
                    "CNAME",
                    format!("{tok}.dkim.amazonses.com"),
                    None,
                ),
            )
        })
        .collect();
    out.push((
        "mail_from".into(),
        r(
            &t.mail_from,
            "MX",
            format!("feedback-smtp.{region}.amazonses.com"),
            Some(10),
        ),
    ));
    out.push((
        "mail_from".into(),
        r(
            &t.mail_from,
            "TXT",
            "v=spf1 include:amazonses.com ~all".into(),
            None,
        ),
    ));
    out
}

/// Compares a wanted record with what's in the zone: "exists", "create" or "conflict".
fn record_state(wanted: &Record, existing: &[Record]) -> (&'static str, Option<String>) {
    let same_name: Vec<&Record> = existing
        .iter()
        .filter(|e| e.name.eq_ignore_ascii_case(&wanted.name))
        .collect();
    if same_name
        .iter()
        .any(|e| e.kind == wanted.kind && same_content(&wanted.kind, &e.content, &wanted.content))
    {
        return ("exists", None);
    }
    // A CNAME can't coexist with anything; a second SPF / MX at the same name would break mail.
    let clash = same_name.iter().find(|e| {
        e.kind == "CNAME"
            || wanted.kind == "CNAME"
            || (wanted.kind == "MX" && e.kind == "MX")
            || (wanted.kind == "TXT"
                && e.kind == "TXT"
                && e.content.trim_matches('"').starts_with("v=spf1"))
    });
    match clash {
        Some(e) => (
            "conflict",
            Some(format!("existing {} record: {}", e.kind, e.content)),
        ),
        None => ("create", None),
    }
}

fn dmarc_present(existing: &[Record], t: &Target) -> bool {
    let sub = format!("_dmarc.{}", rel(&t.identity, &t.zone))
        .trim_end_matches('.')
        .to_string();
    existing.iter().any(|e| {
        e.kind == "TXT"
            && (e.name == "_dmarc" || e.name == sub)
            && e.content.trim_matches('"').starts_with("v=DMARC1")
    })
}

async fn identity_info(aws: &Aws, identity: &str) -> Result<Option<Value>, AwsError> {
    match aws
        .ses("GET", &format!("/v2/email/identities/{identity}"), None)
        .await
    {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.status == 404 || e.code == "NotFoundException" => Ok(None),
        Err(e) => Err(e),
    }
}

fn tokens(identity: &Value) -> Vec<String> {
    identity["DkimAttributes"]["Tokens"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Plan (read-only)

pub async fn plan(
    State(st): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<SetupRequest>,
) -> AppResult<Json<Value>> {
    let t = target(&st, load_site(&st, &user, id).await?)?;
    let aws_int = integrations::load(&st, &user, req.aws_integration_id).await?;
    let aws = aws_int.aws(&st.http, req.region.as_deref())?;
    let dns_int = match req.dns_integration_id {
        Some(d) => Some(integrations::load(&st, &user, d).await?),
        None => None,
    };
    Ok(Json(build_plan(&st, &t, &aws, dns_int.as_ref()).await?))
}

async fn build_plan(
    st: &AppState,
    t: &Target,
    aws: &Aws,
    dns_int: Option<&Integration>,
) -> AppResult<Value> {
    let account = aws
        .ses("GET", "/v2/email/account", None)
        .await
        .map_err(aws_err)?;
    let identity = identity_info(aws, &t.identity).await.map_err(aws_err)?;
    let dkim = identity.as_ref().map(tokens).unwrap_or_default();

    let existing = match dns_int {
        Some(d) => {
            let p = d.dns(&st.http)?;
            p.list(&t.zone).await.map_err(|e| {
                AppError::bad_request(format!(
                    "{e:#}. In Porkbun, enable \"API Access\" for {}.",
                    t.zone
                ))
            })?
        }
        None => Vec::new(),
    };
    let manual = dns_int.is_none();

    let mut steps = vec![json!({
        "id": "identity", "title": format!("Verify {} in Amazon SES (Easy DKIM)", t.identity),
        "action": if identity.is_some() { "exists" } else { "create" },
    })];
    let mut records = Vec::new();
    for (group, rec) in wanted_records(t, &aws.region, &dkim) {
        let (state, note) = if manual {
            ("manual", None)
        } else {
            record_state(&rec, &existing)
        };
        records.push(
            json!({ "group": group, "name": rec.name, "fqdn": rec.fqdn(&t.zone), "type": rec.kind,
            "content": rec.content, "priority": rec.priority, "action": state, "note": note }),
        );
    }
    if dkim.is_empty() {
        records.insert(0, json!({ "group": "dkim", "name": format!("<token>._domainkey.{}", rel(&t.identity, &t.zone)), "type": "CNAME",
            "content": "<token>.dkim.amazonses.com", "action": if manual { "manual" } else { "create" },
            "note": "3 records; values are generated by SES when the identity is created" }));
    }
    let has_dmarc = !manual && dmarc_present(&existing, t);
    records.push(json!({ "group": "dmarc", "name": "_dmarc", "fqdn": format!("_dmarc.{}", t.zone), "type": "TXT",
        "content": "v=DMARC1; p=none", "action": if manual { "manual" } else if has_dmarc { "exists" } else { "create" },
        "note": if has_dmarc { "existing DMARC policy kept" } else { "monitoring-only policy; tighten later" } }));

    // Report what's already in place so a re-run plan reads accurately (all read-only checks).
    let mail_from_done = identity.as_ref().and_then(|i| i["MailFromAttributes"]["MailFromDomain"].as_str()) == Some(t.mail_from.as_str());
    steps.push(json!({ "id": "mail_from", "title": format!("Use {} as the MAIL FROM domain (SPF alignment)", t.mail_from),
        "action": if mail_from_done { "exists" } else { "create" } }));
    let dns_action = if manual {
        "manual"
    } else if records.iter().any(|r| r["action"] == "conflict") {
        "conflict"
    } else if records.iter().all(|r| r["action"] == "exists") {
        "exists"
    } else {
        "create"
    };
    steps.push(json!({ "id": "dns", "title": if manual { "Add the DNS records below at your DNS provider".to_string() } else { format!("Add DNS records in {}", dns_int.map(|d| d.name.as_str()).unwrap_or("")) },
        "action": dns_action }));
    let feedback_done = match aws.ses("GET", &format!("/v2/email/configuration-sets/{}/event-destinations", t.config_set), None).await {
        Ok(v) => v["EventDestinations"].as_array().is_some_and(|d| d.iter().any(|e| e["Name"] == EVENT_DESTINATION)),
        Err(_) => false,
    };
    steps.push(json!({ "id": "feedback", "title": "Send bounces & complaints to Listaculous (configuration set + SNS)",
        "action": if feedback_done { "exists" } else if t.webhook_public { "create" } else { "skip" },
        "note": if t.webhook_public || feedback_done { Value::Null } else { json!(format!("BASE_URL ({}) must be a public https:// URL for AWS to reach the webhook; re-run setup after deploying", st.cfg.base_url)) } }));
    let current = crate::crypto::open_config(&t.site.provider_config).unwrap_or_default();
    let sender_done = t.site.provider_kind == "ses" && current["configuration_set"].as_str() == Some(t.config_set.as_str());
    steps.push(json!({ "id": "sender", "title": format!("Create send-only IAM user {} and switch the site to it", t.iam_user),
        "action": if sender_done { "exists" } else { "create" } }));

    let sandbox = !account["ProductionAccessEnabled"]
        .as_bool()
        .unwrap_or(false);
    Ok(json!({
        "site_id": t.site.id,
        "region": aws.region,
        "zone": t.zone,
        "identity": t.identity,
        "mail_from_domain": t.mail_from,
        "dns": if manual { "manual" } else { "automatic" },
        "steps": steps,
        "records": records,
        "can_apply": !records.iter().any(|r| r["action"] == "conflict"),
        "account": {
            "sandbox": sandbox,
            "max_24_hour_send": account["SendQuota"]["Max24HourSend"],
            "max_send_rate": account["SendQuota"]["MaxSendRate"],
            "review_status": account["Details"]["ReviewDetails"]["Status"],
        },
    }))
}

// ---------------------------------------------------------------------------------------
// Apply (confirmed, background, idempotent)

pub async fn apply(
    State(st): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<SetupRequest>,
) -> AppResult<Json<Value>> {
    if !req.confirm {
        return Err(AppError::bad_request(
            "review the plan first, then call apply with \"confirm\": true",
        ));
    }
    let t = target(&st, load_site(&st, &user, id).await?)?;
    let aws_int = integrations::load(&st, &user, req.aws_integration_id).await?;
    let aws = aws_int.aws(&st.http, req.region.as_deref())?;
    let dns_int = match req.dns_integration_id {
        Some(d) => Some(integrations::load(&st, &user, d).await?),
        None => None,
    };
    // Re-plan right now so we never act on a stale view (and refuse conflicts).
    let plan = build_plan(&st, &t, &aws, dns_int.as_ref()).await?;
    if plan["can_apply"] == false {
        return Err(AppError::bad_request("some DNS records conflict with existing ones; resolve them (see the plan) and try again"));
    }

    // Only one run at a time per site.
    let previous = load_state(&st, id).await?;
    if previous
        .as_ref()
        .is_some_and(|(status, _)| status == "running")
    {
        return Err(AppError::new(
            axum::http::StatusCode::CONFLICT,
            "setup is already running for this site",
        ));
    }
    let mut state = previous.map(|(_, s)| s).unwrap_or_else(|| json!({}));
    state["plan"] = plan;
    state["steps"] = json!({});
    state["error"] = Value::Null;
    state["started_at"] = now().into();
    save_state(&st, id, "running", &state).await?;

    let st2 = st.clone();
    tokio::spawn(async move {
        let dns = dns_int.as_ref().and_then(|d| d.dns(&st2.http).ok());
        let result = run(&st2, &t, &aws, dns.as_deref(), &mut state).await;
        let status = match &result {
            Ok(()) => "applied",
            Err(e) => {
                state["error"] = format!("{e:#}").into();
                "error"
            }
        };
        state["finished_at"] = now().into();
        if let Err(e) = save_state(&st2, t.site.id, status, &state).await {
            tracing::error!("saving setup state for site {}: {e}", t.site.id);
        }
    });
    status_json(&st, id, false, None).await.map(Json)
}

async fn step(
    st: &AppState,
    site_id: i64,
    state: &mut Value,
    id: &str,
    status: &str,
    detail: impl Into<Value>,
) -> anyhow::Result<()> {
    state["steps"][id] = json!({ "status": status, "detail": detail.into(), "at": now() });
    save_state(st, site_id, "running", state).await?;
    Ok(())
}

async fn run(
    st: &AppState,
    t: &Target,
    aws: &Aws,
    dns: Option<&dyn DnsProvider>,
    state: &mut Value,
) -> anyhow::Result<()> {
    let site_id = t.site.id;
    let (account_id, _) = aws.caller_identity().await?;

    // 1. Configuration set (+ feedback to SNS → webhook) so the identity can default to it.
    step(st, site_id, state, "feedback", "running", "").await?;
    match aws
        .ses(
            "POST",
            "/v2/email/configuration-sets",
            Some(json!({ "ConfigurationSetName": t.config_set })),
        )
        .await
    {
        Ok(_) => {}
        Err(e) if e.code == "AlreadyExistsException" => {}
        Err(e) => return Err(e.into()),
    }
    if t.webhook_public {
        let xml = aws
            .query("sns", &[("Action", "CreateTopic"), ("Name", &t.topic)])
            .await?;
        let topic_arn = xml_tag(&xml, "TopicArn")
            .ok_or_else(|| anyhow::anyhow!("CreateTopic returned no TopicArn"))?;
        // Allow SES (from this account only) to publish to the topic.
        let policy = json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Sid": "AllowSESPublish", "Effect": "Allow",
                "Principal": { "Service": "ses.amazonaws.com" },
                "Action": "SNS:Publish", "Resource": topic_arn,
                "Condition": { "StringEquals": { "AWS:SourceAccount": account_id } }
            }]
        })
        .to_string();
        aws.query(
            "sns",
            &[
                ("Action", "SetTopicAttributes"),
                ("TopicArn", &topic_arn),
                ("AttributeName", "Policy"),
                ("AttributeValue", &policy),
            ],
        )
        .await?;
        let subs = aws
            .query(
                "sns",
                &[
                    ("Action", "ListSubscriptionsByTopic"),
                    ("TopicArn", &topic_arn),
                ],
            )
            .await?;
        if !subs.contains(&t.webhook_url.replace('&', "&amp;")) && !subs.contains(&t.webhook_url) {
            aws.query(
                "sns",
                &[
                    ("Action", "Subscribe"),
                    ("TopicArn", &topic_arn),
                    ("Protocol", "https"),
                    ("Endpoint", &t.webhook_url),
                    ("ReturnSubscriptionArn", "true"),
                ],
            )
            .await?;
        }
        let dest = json!({
            "EventDestinationName": EVENT_DESTINATION,
            "EventDestination": { "Enabled": true, "MatchingEventTypes": ["BOUNCE", "COMPLAINT"], "SnsDestination": { "TopicArn": topic_arn } }
        });
        match aws
            .ses(
                "POST",
                &format!(
                    "/v2/email/configuration-sets/{}/event-destinations",
                    t.config_set
                ),
                Some(dest),
            )
            .await
        {
            Ok(_) => {}
            Err(e) if e.code == "AlreadyExistsException" => {}
            Err(e) => return Err(e.into()),
        }
        state["resources"]["topic_arn"] = topic_arn.clone().into();
        step(
            st,
            site_id,
            state,
            "feedback",
            "done",
            format!("bounces & complaints → {topic_arn} → webhook"),
        )
        .await?;
    } else {
        step(
            st,
            site_id,
            state,
            "feedback",
            "skipped",
            "BASE_URL isn't public HTTPS; bounce notifications not connected yet",
        )
        .await?;
    }
    state["resources"]["configuration_set"] = t.config_set.clone().into();

    // 2. SES identity with Easy DKIM, defaulting to the configuration set.
    step(st, site_id, state, "identity", "running", "").await?;
    let created = aws
        .ses(
            "POST",
            "/v2/email/identities",
            Some(json!({ "EmailIdentity": t.identity, "ConfigurationSetName": t.config_set })),
        )
        .await;
    match created {
        Ok(_) => {}
        Err(e) if e.code == "AlreadyExistsException" => {
            aws.ses(
                "PUT",
                &format!("/v2/email/identities/{}/configuration-set", t.identity),
                Some(json!({ "ConfigurationSetName": t.config_set })),
            )
            .await?;
        }
        Err(e) => return Err(e.into()),
    }
    let info = identity_info(aws, &t.identity)
        .await?
        .ok_or_else(|| anyhow::anyhow!("identity missing after creation"))?;
    let dkim = tokens(&info);
    step(
        st,
        site_id,
        state,
        "identity",
        "done",
        format!(
            "{} created; DKIM status {}",
            t.identity,
            info["DkimAttributes"]["Status"].as_str().unwrap_or("?")
        ),
    )
    .await?;

    // 3. Custom MAIL FROM.
    step(st, site_id, state, "mail_from", "running", "").await?;
    aws.ses(
        "PUT",
        &format!("/v2/email/identities/{}/mail-from", t.identity),
        Some(json!({ "MailFromDomain": t.mail_from, "BehaviorOnMxFailure": "USE_DEFAULT_VALUE" })),
    )
    .await?;
    step(st, site_id, state, "mail_from", "done", t.mail_from.clone()).await?;

    // 4. DNS records (create only; never edit or delete).
    let mut wanted: Vec<Record> = wanted_records(t, &aws.region, &dkim)
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    match dns {
        Some(dns) => {
            step(st, site_id, state, "dns", "running", "").await?;
            let existing = dns.list(&t.zone).await?;
            if !dmarc_present(&existing, t) {
                wanted.push(Record {
                    id: String::new(),
                    name: "_dmarc".into(),
                    kind: "TXT".into(),
                    content: "v=DMARC1; p=none".into(),
                    priority: None,
                    ttl: Some(600),
                });
            }
            let mut created = Vec::new();
            for rec in &wanted {
                match record_state(rec, &existing) {
                    ("exists", _) => {}
                    ("conflict", note) => anyhow::bail!(
                        "{} {}: {}",
                        rec.kind,
                        rec.fqdn(&t.zone),
                        note.unwrap_or_default()
                    ),
                    _ => {
                        let id = dns.create(&t.zone, rec).await?;
                        created.push(json!({ "id": id, "fqdn": rec.fqdn(&t.zone), "type": rec.kind, "content": rec.content }));
                    }
                }
            }
            let n = created.len();
            let mut all = state["resources"]["dns_records"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            all.extend(created);
            state["resources"]["dns_records"] = Value::Array(all);
            step(
                st,
                site_id,
                state,
                "dns",
                "done",
                format!("{n} record(s) created"),
            )
            .await?;
        }
        None => {
            step(
                st,
                site_id,
                state,
                "dns",
                "manual",
                "add the records shown in the plan at your DNS provider",
            )
            .await?;
        }
    }

    // 5. Send-only IAM user; its key replaces the site's SES credentials.
    step(st, site_id, state, "sender", "running", "").await?;
    match aws
        .query(
            "iam",
            &[
                ("Action", "CreateUser"),
                ("UserName", &t.iam_user),
                ("Path", "/listaculous/"),
                ("Tags.member.1.Key", "listaculous-site"),
                ("Tags.member.1.Value", &site_id.to_string()),
            ],
        )
        .await
    {
        Ok(_) => {}
        Err(e) if e.code == "EntityAlreadyExists" => {}
        Err(e) => return Err(e.into()),
    }
    let send_policy = json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Action": ["ses:SendEmail", "ses:SendRawEmail"],
            "Resource": [
                format!("arn:aws:ses:{}:{account_id}:identity/{}", aws.region, t.identity),
                format!("arn:aws:ses:{}:{account_id}:configuration-set/{}", aws.region, t.config_set)
            ]
        }]
    })
    .to_string();
    aws.query(
        "iam",
        &[
            ("Action", "PutUserPolicy"),
            ("UserName", &t.iam_user),
            ("PolicyName", "listaculous-send-only"),
            ("PolicyDocument", &send_policy),
        ],
    )
    .await?;

    // Reuse the stored key if the site already has one for this user; otherwise mint one.
    let current = crate::crypto::open_config(&t.site.provider_config)?;
    let have_key = t.site.provider_kind == "ses"
        && current["access_key_id"]
            .as_str()
            .is_some_and(|k| !k.is_empty())
        && state["resources"]["iam_user"].as_str() == Some(t.iam_user.as_str())
        && current["access_key_id"].as_str() == state["resources"]["access_key_id"].as_str();
    let (key_id, secret) = if have_key {
        (
            current["access_key_id"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            current["secret_access_key"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )
    } else {
        let xml = aws.query("iam", &[("Action", "CreateAccessKey"), ("UserName", &t.iam_user)]).await.map_err(|e| {
            if e.code == "LimitExceeded" {
                anyhow::anyhow!("{e} — delete an unused access key of IAM user {} in the AWS console, then re-run", t.iam_user)
            } else {
                e.into()
            }
        })?;
        (
            xml_tag(&xml, "AccessKeyId").ok_or_else(|| anyhow::anyhow!("no AccessKeyId"))?,
            xml_tag(&xml, "SecretAccessKey")
                .ok_or_else(|| anyhow::anyhow!("no SecretAccessKey"))?,
        )
    };
    let mut cfg = json!({
        "region": aws.region,
        "access_key_id": key_id,
        "secret_access_key": secret,
        "configuration_set": t.config_set,
    });
    crate::crypto::seal_config(&mut cfg)?;
    sqlx::query("UPDATE sites SET provider_kind = 'ses', provider_config = $1 WHERE id = $2")
        .bind(cfg.to_string())
        .bind(site_id)
        .execute(&st.db)
        .await?;
    state["resources"]["iam_user"] = t.iam_user.clone().into();
    state["resources"]["access_key_id"] = key_id.clone().into();
    step(
        st,
        site_id,
        state,
        "sender",
        "done",
        format!(
            "site now sends via SES as {} (key {}…; new keys can take ~10s to work)",
            t.iam_user,
            &key_id[..key_id.len().min(8)]
        ),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Status & verification

async fn load_state(st: &AppState, site_id: i64) -> AppResult<Option<(String, Value)>> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT status, state FROM site_setups WHERE site_id = $1")
            .bind(site_id)
            .fetch_optional(&st.db)
            .await?;
    Ok(row.map(|(s, j)| (s, serde_json::from_str(&j).unwrap_or_default())))
}

async fn save_state(
    st: &AppState,
    site_id: i64,
    status: &str,
    state: &Value,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO site_setups (site_id, status, state, updated_at) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (site_id) DO UPDATE SET status = excluded.status, state = excluded.state, updated_at = excluded.updated_at",
    )
    .bind(site_id)
    .bind(status)
    .bind(state.to_string())
    .bind(now())
    .execute(&st.db)
    .await?;
    Ok(())
}

async fn status_json(
    st: &AppState,
    site_id: i64,
    verify: bool,
    aws: Option<&Aws>,
) -> AppResult<Value> {
    let Some((status, state)) = load_state(st, site_id).await? else {
        return Ok(json!({ "status": "not_started" }));
    };
    let mut out = json!({ "status": status, "state": state });
    if verify {
        if let (Some(aws), Some(identity)) = (aws, state["plan"]["identity"].as_str()) {
            if let Some(info) = identity_info(aws, identity).await.map_err(aws_err)? {
                out["verification"] = json!({
                    "dkim": info["DkimAttributes"]["Status"],
                    "mail_from": info["MailFromAttributes"]["MailFromDomainStatus"],
                    "verified_for_sending": info["VerifiedForSendingStatus"],
                });
            }
        }
    }
    Ok(out)
}

#[derive(Deserialize, Default)]
pub struct StatusQuery {
    /// When set, live-checks DKIM / MAIL FROM verification with this AWS integration.
    #[serde(default)]
    pub aws_integration_id: Option<i64>,
}

pub async fn status(
    State(st): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    axum::extract::Query(q): axum::extract::Query<StatusQuery>,
) -> AppResult<Json<Value>> {
    load_site(&st, &user, id).await?;
    let aws = match q.aws_integration_id {
        Some(i) => {
            let region = load_state(&st, id)
                .await?
                .and_then(|(_, s)| s["plan"]["region"].as_str().map(str::to_string));
            Some(
                integrations::load(&st, &user, i)
                    .await?
                    .aws(&st.http, region.as_deref())?,
            )
        }
        None => None,
    };
    status_json(&st, id, aws.is_some(), aws.as_ref())
        .await
        .map(Json)
}

// ---------------------------------------------------------------------------------------
// Production access (leaving the SES sandbox)

pub async fn production_access_draft(
    State(st): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let site = load_site(&st, &user, id).await?;
    let (lists,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM lists WHERE site_id = $1")
        .bind(id)
        .fetch_one(&st.db)
        .await?;
    Ok(Json(json!({
        "mail_type": "MARKETING",
        "website_url": format!("https://{}", site.domain),
        "contact_emails": [user.email],
        "use_case_description": format!(
"We send newsletters for {name} ({domain}) to people who subscribed on our website.

How recipients join: every signup uses double opt-in — the address only receives mail after clicking a confirmation link we email to it. We record the time, IP address and signup source as proof of consent. We never buy, rent or import lists without consent.

What we send: newsletter updates (typically weekly to monthly) to {lists} list(s). Volume grows with our subscriber base.

Unsubscribes: every message includes a visible unsubscribe link and RFC 8058 one-click List-Unsubscribe headers, our physical mailing address, and a plain-text part. Unsubscribes take effect immediately.

Bounces and complaints: SES bounce and complaint notifications are delivered via an SES configuration set and SNS to our sending software (Listaculous), which automatically suppresses hard-bounced and complaining addresses across all our lists so they are never emailed again.

Authentication: the sending domain uses Easy DKIM, a custom MAIL FROM domain for SPF alignment, and a DMARC policy.",
            name = site.name, domain = site.domain, lists = lists.max(1)),
    })))
}

#[derive(Deserialize)]
pub struct ProductionAccess {
    pub aws_integration_id: i64,
    #[serde(default)]
    pub region: Option<String>,
    pub website_url: String,
    pub use_case_description: String,
    #[serde(default)]
    pub contact_emails: Vec<String>,
    #[serde(default = "marketing")]
    pub mail_type: String,
}
fn marketing() -> String {
    "MARKETING".into()
}

/// Submits the SES production access request (reviewed by AWS, usually within ~24h).
pub async fn request_production_access(
    State(st): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(p): Json<ProductionAccess>,
) -> AppResult<Json<Value>> {
    load_site(&st, &user, id).await?;
    if p.use_case_description.trim().len() < 50 {
        return Err(AppError::bad_request(
            "describe your use case in more detail (AWS rejects short descriptions)",
        ));
    }
    if !matches!(p.mail_type.as_str(), "MARKETING" | "TRANSACTIONAL") {
        return Err(AppError::bad_request(
            "mail_type must be MARKETING or TRANSACTIONAL",
        ));
    }
    let aws = integrations::load(&st, &user, p.aws_integration_id)
        .await?
        .aws(&st.http, p.region.as_deref())?;
    aws.ses(
        "POST",
        "/v2/email/account/details",
        Some(json!({
            "MailType": p.mail_type,
            "WebsiteURL": p.website_url,
            "UseCaseDescription": p.use_case_description,
            "AdditionalContactEmailAddresses": p.contact_emails,
            "ContactLanguage": "EN",
            "ProductionAccessEnabled": true,
        })),
    )
    .await
    .map_err(aws_err)?;
    let account = aws
        .ses("GET", "/v2/email/account", None)
        .await
        .map_err(aws_err)?;
    Ok(Json(
        json!({ "ok": true, "review_status": account["Details"]["ReviewDetails"]["Status"], "region": aws.region }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, kind: &str, content: &str) -> Record {
        Record {
            id: "1".into(),
            name: name.into(),
            kind: kind.into(),
            content: content.into(),
            priority: None,
            ttl: None,
        }
    }

    #[test]
    fn detects_existing_and_conflicting_records() {
        let existing = vec![
            rec(
                "bounce.news",
                "TXT",
                "\"v=spf1 include:amazonses.com ~all\"",
            ),
            rec("abc._domainkey.news", "CNAME", "other.example.net"),
            rec("news", "A", "1.2.3.4"),
        ];
        let want_spf = rec("bounce.news", "TXT", "v=spf1 include:amazonses.com ~all");
        assert_eq!(record_state(&want_spf, &existing).0, "exists");
        let want_dkim = rec("abc._domainkey.news", "CNAME", "abc.dkim.amazonses.com");
        assert_eq!(record_state(&want_dkim, &existing).0, "conflict");
        let want_mx = rec("bounce.news", "MX", "feedback-smtp.us-east-1.amazonses.com");
        assert_eq!(
            record_state(&want_mx, &existing).0,
            "create",
            "an SPF TXT doesn't block an MX"
        );
        let other_spf = vec![rec("bounce.news", "TXT", "v=spf1 -all")];
        assert_eq!(
            record_state(&want_spf, &other_spf).0,
            "conflict",
            "never add a second SPF record"
        );
    }

    #[test]
    fn relative_names() {
        assert_eq!(rel("bounce.news.example.com", "example.com"), "bounce.news");
        assert_eq!(rel("example.com", "example.com"), "");
    }
}
