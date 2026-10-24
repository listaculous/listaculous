//! Automatic Amazon SES + DNS setup for a site.
//!
//! `plan` is read-only: it inspects AWS and DNS and lists every change. `apply` (explicitly
//! confirmed) performs them idempotently in the background, recording progress so it can be
//! re-run safely. It never edits DNS records and deletes only one kind: an SES MAIL FROM MX left
//! by setup in another region, which it replaces when setup moves regions. Existing matching
//! records are reused; any other conflicting one stops the run. After a region move, what the old
//! region used is remembered so it can be cleaned up (see `cleanup`).
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

/// Policy for a site's send-only IAM user. SES authorizes a send against every identity involved,
/// including verified *recipient* identities (as in the sandbox, where every recipient must be
/// verified), so the policy can't name just the sender's identity. Instead it allows any identity
/// in the account and region, but only with a From address on the site's own domain.
fn send_policy(region: &str, account_id: &str, identity: &str, config_set: &str) -> String {
    json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Action": ["ses:SendEmail", "ses:SendRawEmail"],
            "Resource": [
                format!("arn:aws:ses:{region}:{account_id}:identity/*"),
                format!("arn:aws:ses:{region}:{account_id}:configuration-set/{config_set}")
            ],
            "Condition": { "StringLike": { "ses:FromAddress": format!("*@{identity}") } }
        }]
    })
    .to_string()
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

fn target(st: &AppState, site: Site, region: &str) -> AppResult<Target> {
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
        // Namespaced by installation so several installs can share one AWS account.
        config_set: format!("listaculous-{}-site-{}", st.instance_id, site.id),
        topic: format!("listaculous-{}-site-{}-feedback", st.instance_id, site.id),
        // One sender per region: re-running setup in another region never changes (or breaks)
        // the permissions of the user the site may still be sending with.
        // (and per sending identity, so a run for an old identity can't alter the active sender).
        iam_user: format!(
            "listaculous-{}-s{}-{region}-{}",
            st.instance_id,
            site.id,
            &crate::db::sha256_hex(&identity)[..8]
        ),
        zone,
        identity,
        webhook_url,
        webhook_public,
        site,
    })
}

async fn load_site(st: &AppState, user: &AuthUser, id: i64) -> AppResult<Site> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SITE_COLS} FROM sites WHERE id = $1 AND account_id = $2"
    )))
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

fn dest_body(topic_arn: &str) -> Value {
    json!({ "Enabled": true, "MatchingEventTypes": ["BOUNCE", "COMPLAINT"], "SnsDestination": { "TopicArn": topic_arn } })
}

/// Whether our event destination exists, is enabled, covers bounces + complaints and targets the topic.
fn destination_ok(destinations: &Value, topic_arn: &str) -> bool {
    destinations["EventDestinations"]
        .as_array()
        .is_some_and(|all| {
            all.iter().any(|d| {
                let types: Vec<&str> = d["MatchingEventTypes"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
                    .unwrap_or_default();
                d["Name"] == EVENT_DESTINATION
                    && d["Enabled"].as_bool().unwrap_or(false)
                    && types.contains(&"BOUNCE")
                    && types.contains(&"COMPLAINT")
                    && d["SnsDestination"]["TopicArn"].as_str() == Some(topic_arn)
            })
        })
}

fn dmarc_record() -> Record {
    Record {
        id: String::new(),
        name: "_dmarc".into(),
        kind: "TXT".into(),
        content: "v=DMARC1; p=none".into(),
        priority: None,
        ttl: Some(600),
    }
}

/// The records SES needs, given the DKIM tokens (if the identity exists yet).
/// Where Easy DKIM CNAMEs point: the zone SES reports for the identity, else the region's
/// documented DKIM domain (newer regions use regional ones; see AWS "SES endpoints: DKIM domains").
fn dkim_domain(region: &str, identity: Option<&Value>) -> String {
    if let Some(z) = identity
        .and_then(|i| i["DkimAttributes"]["SigningHostedZone"].as_str())
        .filter(|z| !z.is_empty())
    {
        return z.trim_end_matches('.').to_string();
    }
    const REGIONAL: &[&str] = &[
        "af-south-1",
        "ap-south-2",
        "ap-southeast-3",
        "ap-southeast-5",
        "ca-west-1",
        "ap-northeast-3",
        "eu-south-1",
        "eu-central-2",
        "il-central-1",
        "me-central-1",
        "us-gov-east-1",
    ];
    if REGIONAL.contains(&region) {
        format!("dkim.{region}.amazonses.com")
    } else {
        "dkim.amazonses.com".into()
    }
}

fn wanted_records(
    t: &Target,
    region: &str,
    dkim_domain: &str,
    dkim_tokens: &[String],
) -> Vec<(String, Record)> {
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
                    format!("{tok}.{dkim_domain}"),
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

/// Compares a wanted record with *all* records at its name: "exists", "create" or "conflict".
/// A matching record doesn't hide an incompatible neighbour (e.g. a second MX or SPF).
fn record_state(wanted: &Record, existing: &[Record]) -> (&'static str, Option<String>) {
    let matches = |e: &Record| {
        e.kind == wanted.kind
            && same_content(&wanted.kind, &e.content, &wanted.content)
            && (wanted.kind != "MX" || e.priority.unwrap_or(0) == wanted.priority.unwrap_or(0))
    };
    let same_name: Vec<&Record> = existing
        .iter()
        .filter(|e| e.name.eq_ignore_ascii_case(&wanted.name))
        .collect();
    // A CNAME can't coexist with anything; a second MX or SPF at the same name would break mail.
    let clash = same_name.iter().find(|e| {
        !matches(e)
            && (e.kind == "CNAME"
                || wanted.kind == "CNAME"
                || (wanted.kind == "MX" && e.kind == "MX")
                || (wanted.kind == "TXT"
                    && e.kind == "TXT"
                    && e.content.trim_matches('"').starts_with("v=spf1")))
    });
    if let Some(e) = clash {
        let prio = if e.kind == "MX" {
            format!("{} ", e.priority.unwrap_or(0))
        } else {
            String::new()
        };
        return (
            "conflict",
            Some(format!("existing {} record: {prio}{}", e.kind, e.content)),
        );
    }
    if same_name.iter().any(|e| matches(e)) {
        ("exists", None)
    } else {
        ("create", None)
    }
}

/// Existing MX records at the wanted MX's name that are SES MAIL FROM endpoints for *another*
/// region (`feedback-smtp.<region>.amazonses.com`): what setup in a previous region left behind.
fn stale_ses_mx<'a>(wanted: &Record, existing: &'a [Record], region: &str) -> Vec<&'a Record> {
    if wanted.kind != "MX" {
        return Vec::new();
    }
    existing
        .iter()
        .filter(|e| {
            let content = e.content.trim().trim_end_matches('.').to_ascii_lowercase();
            e.kind == "MX"
                && e.name.eq_ignore_ascii_case(&wanted.name)
                && content
                    .strip_prefix("feedback-smtp.")
                    .and_then(|r| r.strip_suffix(".amazonses.com"))
                    .is_some_and(|r| {
                        r != region
                            && !r.is_empty()
                            && r.chars()
                                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    })
        })
        .collect()
}

/// What to do with a wanted record: like `record_state`, except that a clash caused only by
/// another region's SES MAIL FROM MX *that setup created* (`created_ids`) becomes "replace", unless
/// another site sends from the same domain (`shared_with`) and may still rely on it. Returns the
/// records to delete, and whether the wanted record must be created (it may exist already).
fn record_plan<'a>(
    wanted: &Record,
    existing: &'a [Record],
    region: &str,
    created_ids: &[String],
    shared_with: &[String],
) -> (&'static str, Option<String>, Vec<&'a Record>, bool) {
    let (state, note) = record_state(wanted, existing);
    let stale = stale_ses_mx(wanted, existing, region);
    if stale.is_empty() {
        return (state, note, stale, state == "create");
    }
    let old: Vec<String> = stale
        .iter()
        .map(|r| format!("MX {} {}", r.priority.unwrap_or(0), r.content))
        .collect();
    let old = old.join(", ");
    if !shared_with.is_empty() {
        return ("conflict", Some(format!("{old} (another SES region) is also used by {}; move them together or change it by hand", shared_with.join(", "))), Vec::new(), false);
    }
    if stale.iter().any(|r| !created_ids.contains(&r.id)) {
        return ("conflict", Some(format!("{old} points to another SES region but wasn't created by Listaculous; delete it if nothing else uses it, then preview again")), Vec::new(), false);
    }
    let rest: Vec<Record> = existing
        .iter()
        .filter(|e| !stale.iter().any(|s| std::ptr::eq(*s, *e)))
        .cloned()
        .collect();
    match record_state(wanted, &rest) {
        ("conflict", note) => ("conflict", note, Vec::new(), false),
        (after, _) => (
            "replace",
            Some(format!("replaces {old} (MAIL FROM for another SES region)")),
            stale,
            after == "create",
        ),
    }
}

/// Provider ids of the DNS records setup has created for this site.
fn created_record_ids(state: &Value) -> Vec<String> {
    state["resources"]["dns_records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect()
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
    let site = load_site(&st, &user, id).await?;
    let aws_int = integrations::load(&st, &user, req.aws_integration_id).await?;
    let aws = aws_int.aws(&st.http, req.region.as_deref())?;
    let t = target(&st, site, &aws.region)?;
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
    let saved_state = load_state(st, t.site.id)
        .await?
        .map(|(_, s, _)| s)
        .unwrap_or_default();
    let created_ids = created_record_ids(&saved_state);
    let shared = sites_sharing(st, t.site.account_id, t.site.id, &t.identity).await?;

    let mut steps = vec![json!({
        "id": "identity", "title": format!("Verify {} in Amazon SES (Easy DKIM)", t.identity),
        "action": if identity.is_some() { "exists" } else { "create" },
    })];
    let mut records = Vec::new();
    let dkim_target = dkim_domain(&aws.region, identity.as_ref());
    for (group, rec) in wanted_records(t, &aws.region, &dkim_target, &dkim) {
        let (state, note) = if manual {
            ("manual", None)
        } else {
            let (state, note, _, _) =
                record_plan(&rec, &existing, &aws.region, &created_ids, &shared);
            (state, note)
        };
        records.push(
            json!({ "group": group, "name": rec.name, "fqdn": rec.fqdn(&t.zone), "type": rec.kind,
            "content": rec.content, "priority": rec.priority, "action": state, "note": note }),
        );
    }
    if dkim.is_empty() {
        records.insert(0, json!({ "group": "dkim", "name": format!("<token>._domainkey.{}", rel(&t.identity, &t.zone)), "type": "CNAME",
            "content": format!("<token>.{dkim_target}"), "action": if manual { "manual" } else { "create" },
            "note": "3 records; values are generated by SES when the identity is created" }));
    }
    let dmarc = dmarc_record();
    let (dmarc_action, dmarc_note) = if manual {
        (
            "manual",
            format!(
                "only if {} has no DMARC record yet; a second one invalidates DMARC",
                t.zone
            ),
        )
    } else if dmarc_present(&existing, t) {
        ("exists", "existing DMARC policy kept".to_string())
    } else {
        match record_state(&dmarc, &existing) {
            ("conflict", note) => ("conflict", note.unwrap_or_default()),
            (a, _) => (a, "monitoring-only policy; tighten later".to_string()),
        }
    };
    records.push(
        json!({ "group": "dmarc", "name": dmarc.name, "fqdn": dmarc.fqdn(&t.zone), "type": "TXT",
        "content": dmarc.content, "action": dmarc_action, "note": dmarc_note }),
    );

    // Report what's already in place so a re-run plan reads accurately (all read-only checks).
    let mail_from_done = identity
        .as_ref()
        .and_then(|i| i["MailFromAttributes"]["MailFromDomain"].as_str())
        == Some(t.mail_from.as_str());
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
    let (account_id, _) = aws
        .caller_identity()
        .await
        .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    let topic_arn = format!("arn:aws:sns:{}:{account_id}:{}", aws.region, t.topic);
    let feedback_done = match aws
        .ses(
            "GET",
            &format!(
                "/v2/email/configuration-sets/{}/event-destinations",
                t.config_set
            ),
            None,
        )
        .await
    {
        Ok(v) => destination_ok(&v, &topic_arn),
        Err(_) => false,
    };
    // Only "done" if SNS will actually deliver: the webhook subscription must be confirmed.
    let feedback_done = feedback_done
        && match aws
            .query(
                "sns",
                &[
                    ("Action", "ListSubscriptionsByTopic"),
                    ("TopicArn", &topic_arn),
                ],
            )
            .await
        {
            Ok(xml) => subscription_state(&xml, &t.webhook_url) == SubState::Confirmed,
            Err(_) => false,
        };
    steps.push(json!({ "id": "feedback", "title": "Send bounces & complaints to Listaculous (configuration set + SNS)",
        "action": if feedback_done { "exists" } else if t.webhook_public { "create" } else { "skip" },
        "note": if t.webhook_public || feedback_done { Value::Null } else { json!(format!("BASE_URL ({}) must be a public https:// URL for AWS to reach the webhook; re-run setup after deploying", st.cfg.base_url)) } }));
    let current = crate::crypto::open_config(&t.site.provider_config).unwrap_or_default();
    // "exists" only if the site already sends with the key this setup created for *this* AWS
    // account, region and identity (the config set name alone doesn't change across those).
    let slot = &saved_state["resources"]["sender_keys"][format!("{account_id}/{}", t.iam_user)];
    let sender_done = t.site.provider_kind == "ses"
        && current["configuration_set"].as_str() == Some(t.config_set.as_str())
        && current["region"].as_str() == Some(aws.region.as_str())
        && slot["access_key_id"].as_str().is_some()
        && current["access_key_id"].as_str() == slot["access_key_id"].as_str();
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
    let site = load_site(&st, &user, id).await?;
    let aws_int = integrations::load(&st, &user, req.aws_integration_id).await?;
    let aws = aws_int.aws(&st.http, req.region.as_deref())?;
    let t = target(&st, site, &aws.region)?;
    let dns_int = match req.dns_integration_id {
        Some(d) => Some(integrations::load(&st, &user, d).await?),
        None => None,
    };
    // Re-plan right now so we never act on a stale view (and refuse conflicts).
    let plan = build_plan(&st, &t, &aws, dns_int.as_ref()).await?;
    if plan["can_apply"] == false {
        return Err(AppError::bad_request("some DNS records conflict with existing ones; resolve them (see the plan) and try again"));
    }

    // Atomically claim the run first (one at a time per site; a "running" row that hasn't been
    // touched within the lease is abandoned and can be taken over). Only then read the state, so
    // we build on the latest saved resources rather than a snapshot another run may have updated.
    let run_id = crate::db::random_token(12);
    if !claim_run(&st, id, &run_id).await? {
        return Err(AppError::new(
            axum::http::StatusCode::CONFLICT,
            "setup is already running for this site",
        ));
    }
    let mut state = load_state(&st, id)
        .await?
        .map(|(_, s, _)| s)
        .unwrap_or_else(|| json!({}));
    adopt_legacy_active(&mut state, &t.site);
    state["plan"] = plan;
    state["request"] = json!({ "aws_integration_id": req.aws_integration_id, "dns_integration_id": req.dns_integration_id, "region": aws.region });
    state["steps"] = json!({});
    state["error"] = Value::Null;
    state["started_at"] = now().into();
    save_state(&st, id, &run_id, "running", &state).await?;

    let st2 = st.clone();
    tokio::spawn(async move {
        let dns = dns_int.as_ref().and_then(|d| d.dns(&st2.http).ok());
        let result = run(&st2, &t, &aws, dns.as_deref(), &run_id, &mut state).await;
        let status = match &result {
            Ok(()) => "applied",
            Err(e) => {
                state["error"] = format!("{e:#}").into();
                "error"
            }
        };
        state["finished_at"] = now().into();
        if let Err(e) = save_state(&st2, t.site.id, &run_id, status, &state).await {
            tracing::error!("saving setup state for site {}: {e}", t.site.id);
        }
    });
    status_json(&st, id, false, None).await.map(Json)
}

// Region moves. After each successful run, `state.active` records what the site now sends
// through (AWS account, region, identity and what setup created for it). When a later run switches
// the site to a different account/region/identity, the old record moves to `state.previous`, keyed
// by `account/region/identity`, so it can be cleaned up. Returning to a previous setup takes its
// record back. Failed runs change neither, so `previous` never describes what's sending.

fn setup_key(v: &Value) -> String {
    format!(
        "{}/{}/{}",
        v["aws_account_id"].as_str().unwrap_or_default(),
        v["region"].as_str().unwrap_or_default(),
        v["identity"].as_str().unwrap_or_default()
    )
}

/// DKIM CNAMEs (created by setup) that belong to `identity`.
fn dkim_records_of(records: &Value, identity: &str) -> Vec<Value> {
    let suffix = format!("._domainkey.{identity}");
    records
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| {
            r["type"] == "CNAME" && r["fqdn"].as_str().is_some_and(|f| f.ends_with(&suffix))
        })
        .cloned()
        .collect()
}

/// Adds records to `into` that it doesn't have yet (by provider id, else by name).
fn merge_records(into: &mut Value, more: &Value) {
    let mut all = into.as_array().cloned().unwrap_or_default();
    for r in more.as_array().into_iter().flatten() {
        // Records with provider ids are the same only if the ids match (a replacement at the same
        // name is a different record); without ids, by name and type.
        let dup = all
            .iter()
            .any(|e| match (r["id"].as_str(), e["id"].as_str()) {
                (Some(a), Some(b)) => a == b,
                _ => e["fqdn"] == r["fqdn"] && e["type"] == r["type"],
            });
        if !dup {
            all.push(r.clone());
        }
    }
    *into = Value::Array(all);
}

/// Notes, as soon as it happens, that a run created something for a setup (identity or DKIM
/// records), so a run that fails later doesn't lose it: the next successful one picks it up.
fn note_owned(state: &mut Value, key: &str, identity_created: bool, dns: &[Value]) {
    if !state["owned"].is_object() {
        state["owned"] = json!({});
    }
    let o = &mut state["owned"][key];
    if identity_created {
        o["identity_created"] = true.into();
    }
    merge_records(&mut o["dns_records"], &Value::Array(dns.to_vec()));
}

/// Setups from before `state.active` existed: if the site still sends with the key the saved
/// resources name, that's the active setup (whether setup created its identity is unknown).
fn adopt_legacy_active(state: &mut Value, site: &Site) {
    if state["active"].is_object() || site.provider_kind != "ses" {
        return;
    }
    let r = &state["resources"];
    let current = crate::crypto::open_config(&site.provider_config).unwrap_or_default();
    let (Some(key), Some(region), Some(identity)) = (
        r["access_key_id"].as_str(),
        current["region"].as_str(),
        state["plan"]["identity"].as_str(),
    ) else {
        return;
    };
    if current["access_key_id"].as_str() != Some(key) {
        return;
    }
    state["active"] = json!({
        "aws_account_id": r["aws_account_id"], "region": region, "identity": identity,
        "zone": site.domain.trim().to_ascii_lowercase(),
        "configuration_set": r["configuration_set"], "topic_arn": r["topic_arn"],
        "iam_user": r["iam_user"], "access_key_id": key,
        "identity_created": Value::Null,
        "dns_records": dkim_records_of(&r["dns_records"], identity),
    });
}

/// Records `new` as the active setup after a successful run. A re-run of the same setup keeps
/// what it knew; a different one moves the old record to `previous`; returning to a previous one
/// takes its record back.
fn activate(state: &mut Value, mut new: Value) {
    let key = setup_key(&new);
    let absorb = |new: &mut Value, old: &Value| {
        merge_records(&mut new["dns_records"], &old["dns_records"]);
        // Created by setup once means it stays setup's to clean up; otherwise unknown stays unknown.
        new["identity_created"] =
            if new["identity_created"] == true || old["identity_created"] == true {
                true.into()
            } else if old["identity_created"].is_null() {
                Value::Null
            } else {
                false.into()
            };
    };
    // What earlier (possibly failed) runs created for this setup.
    if let Some(owned) = state["owned"].as_object_mut().and_then(|m| m.remove(&key)) {
        merge_records(&mut new["dns_records"], &owned["dns_records"]);
        if owned["identity_created"] == true {
            new["identity_created"] = true.into();
        }
    }
    if let Some(prev) = state["previous"]
        .as_object_mut()
        .and_then(|m| m.remove(&key))
    {
        absorb(&mut new, &prev);
    }
    let old = state["active"].take();
    if old.is_object() {
        if setup_key(&old) == key {
            absorb(&mut new, &old);
        } else {
            if !state["previous"].is_object() {
                state["previous"] = json!({});
            }
            let old_key = setup_key(&old);
            let mut old = old;
            old["archived_at"] = now().into();
            state["previous"][old_key] = old;
        }
    }
    state["active"] = new;
}

/// Whether a previous setup's SES identity (and its DKIM records) may be removed: only if setup
/// created it (or that's unknown, for old setups) and no other site sends from that domain.
fn identity_removable(p: &Value, shared_with: &[String]) -> bool {
    shared_with.is_empty() && p["identity_created"] != false
}

/// AWS CLI commands that remove what setup created for a previous setup, as one block that first
/// checks it's signed in to that setup's AWS account (resource names repeat across accounts).
/// The configuration set and SNS topic are per site and region: `others` are the site's other
/// setups (active and previous), and while any of them is in the same account and region (only
/// the sending identity differed), those two are kept for the last one's cleanup.
fn cleanup_commands(p: &Value, others: &[&Value], shared_with: &[String]) -> Vec<String> {
    let region = p["region"].as_str().unwrap_or_default();
    let account = p["aws_account_id"].as_str().unwrap_or_default();
    let same_place = others
        .iter()
        .any(|o| o["aws_account_id"] == p["aws_account_id"] && o["region"] == p["region"]);
    let mut out = vec![
        "(set -e".to_string(),
        format!(
            "[ \"$(aws sts get-caller-identity --query Account --output text)\" = \"{account}\" ] \
             || {{ echo \"Sign in to AWS account {account} first\" >&2; exit 1; }}"
        ),
        // Already deleted (an earlier, partial run of this block, or another cleanup) is fine;
        // any other failure stops the block.
        "del() { out=$(\"$@\" 2>&1) && return 0; case \"$out\" in *NoSuchEntity*|*NotFoundException*|*NotFound*) echo \"already gone: $*\";; *) echo \"$out\" >&2; return 1;; esac; }".to_string(),
    ];
    // The old send-only key first: revoking it matters most.
    if let Some(user) = p["iam_user"].as_str() {
        if let Some(key) = p["access_key_id"].as_str() {
            out.push(format!(
                "del aws iam delete-access-key --user-name {user} --access-key-id {key}"
            ));
        }
        out.push(format!(
            "del aws iam delete-user-policy --user-name {user} --policy-name listaculous-send-only"
        ));
        out.push(format!("del aws iam delete-user --user-name {user}"));
    }
    // The identity, then its configuration set and topic: setup made that configuration set the
    // identity's default, so they go only with the identity (SES rejects sends through an
    // identity whose default configuration set is gone), and never while the active setup
    // (same account and region) still uses them.
    let identity = p["identity"].as_str().unwrap_or_default();
    let mut group = Vec::new();
    if !identity.is_empty() {
        group.push(format!(
            "del aws sesv2 delete-email-identity --email-identity {identity} --region {region}"
        ));
    }
    if !same_place {
        if let Some(cs) = p["configuration_set"].as_str() {
            group.push(format!(
                "del aws sesv2 delete-configuration-set --configuration-set-name {cs} --region {region}"
            ));
        }
        if let Some(arn) = p["topic_arn"].as_str() {
            group.push(format!(
                "del aws sns delete-topic --topic-arn {arn} --region {region}"
            ));
        }
    }
    if !identity_removable(p, shared_with) {
        out.push(format!(
            "# Kept: {identity} in {region} and its configuration set (still used elsewhere, or not created by Listaculous)"
        ));
    } else if p["identity_created"].is_null() {
        // Old setups didn't record whether they created the identity: leave that call to you.
        out.push(format!(
            "# Only if nothing else sends as {identity} in {region}, uncomment:"
        ));
        out.extend(group.into_iter().map(|c| format!("# {c}")));
    } else {
        out.extend(group);
    }
    out.push(")".into());
    out
}

/// The site's current SES settings (`region`, `access_key_id`, …), if it sends with SES.
async fn sending_config(st: &AppState, site_id: i64) -> AppResult<Option<Value>> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT provider_kind, provider_config FROM sites WHERE id = $1")
            .bind(site_id)
            .fetch_optional(&st.db)
            .await?;
    Ok(row.and_then(|(kind, cfg)| {
        (kind == "ses")
            .then(|| crate::crypto::open_config(&cfg).ok())
            .flatten()
    }))
}

/// Whether the site may still depend on a previous setup: it sends with that setup's key, or
/// through that region at all (e.g. switched back by hand with another key). Conservative: such
/// a setup is neither listed nor cleaned up.
fn in_use(p: &Value, current: Option<&Value>) -> bool {
    current.is_some_and(|c| {
        (p["access_key_id"].is_string() && c["access_key_id"] == p["access_key_id"])
            || (p["region"].is_string() && c["region"] == p["region"])
    })
}

/// Other sites of the account that send from `identity` or a subdomain of it.
async fn sites_sharing(
    st: &AppState,
    account_id: i64,
    site_id: i64,
    identity: &str,
) -> AppResult<Vec<String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT name, from_email FROM sites WHERE account_id = $1 AND id <> $2")
            .bind(account_id)
            .bind(site_id)
            .fetch_all(&st.db)
            .await?;
    Ok(rows
        .into_iter()
        .filter(|(_, from)| {
            // Sending from the domain or any subdomain of it can rely on the identity (SES
            // verifies subdomains through a parent domain identity).
            from.rsplit('@').next().is_some_and(|d| {
                let d = d.to_ascii_lowercase();
                let identity = identity.to_ascii_lowercase();
                d == identity || d.ends_with(&format!(".{identity}"))
            })
        })
        .map(|(name, _)| name)
        .collect())
}

async fn step(
    st: &AppState,
    site_id: i64,
    run_id: &str,
    state: &mut Value,
    id: &str,
    status: &str,
    detail: impl Into<Value>,
) -> anyhow::Result<()> {
    state["steps"][id] = json!({ "status": status, "detail": detail.into(), "at": now() });
    save_state(st, site_id, run_id, "running", state).await?;
    Ok(())
}

async fn run(
    st: &AppState,
    t: &Target,
    aws: &Aws,
    dns: Option<&dyn DnsProvider>,
    run_id: &str,
    state: &mut Value,
) -> anyhow::Result<()> {
    let site_id = t.site.id;
    let (account_id, _) = aws.caller_identity().await?;

    // 1. Configuration set (+ feedback to SNS → webhook) so the identity can default to it.
    step(st, site_id, run_id, state, "feedback", "running", "").await?;
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
        // HTTPS subscriptions deliver nothing until the endpoint confirms them, so a pending one
        // is (re)subscribed, which makes SNS resend the confirmation request.
        let sub_state = subscription_state(&subs, &t.webhook_url);
        if sub_state != SubState::Confirmed {
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
        let dest = json!({ "EventDestinationName": EVENT_DESTINATION, "EventDestination": dest_body(&topic_arn) });
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
            // Exists already: make sure it's enabled, covers bounces + complaints and targets our topic.
            Err(e) if e.code == "AlreadyExistsException" => {
                let current = aws
                    .ses(
                        "GET",
                        &format!(
                            "/v2/email/configuration-sets/{}/event-destinations",
                            t.config_set
                        ),
                        None,
                    )
                    .await?;
                if !destination_ok(&current, &topic_arn) {
                    aws.ses(
                        "PUT",
                        &format!("/v2/email/configuration-sets/{}/event-destinations/{EVENT_DESTINATION}", t.config_set),
                        Some(json!({ "EventDestination": dest_body(&topic_arn) })),
                    )
                    .await?;
                }
            }
            Err(e) => return Err(e.into()),
        }
        state["resources"]["topic_arn"] = topic_arn.clone().into();
        let detail = if sub_state == SubState::Confirmed {
            format!("bounces & complaints → {topic_arn} → webhook")
        } else {
            format!("bounces & complaints → {topic_arn}; waiting for the webhook to confirm the SNS subscription (needs {} to be reachable from the internet). Re-run setup if this doesn't clear.", st.cfg.base_url)
        };
        step(
            st,
            site_id,
            run_id,
            state,
            "feedback",
            if sub_state == SubState::Confirmed {
                "done"
            } else {
                "pending"
            },
            detail,
        )
        .await?;
    } else {
        step(
            st,
            site_id,
            run_id,
            state,
            "feedback",
            "skipped",
            "BASE_URL isn't public HTTPS; bounce notifications not connected yet",
        )
        .await?;
    }
    state["resources"]["configuration_set"] = t.config_set.clone().into();

    // 2. SES identity with Easy DKIM, defaulting to the configuration set.
    step(st, site_id, run_id, state, "identity", "running", "").await?;
    let created = aws
        .ses(
            "POST",
            "/v2/email/identities",
            Some(json!({ "EmailIdentity": t.identity, "ConfigurationSetName": t.config_set })),
        )
        .await;
    // Whether this setup created the identity (so cleanup may later remove it) or found it.
    let identity_created = created.is_ok();
    let setup_id = setup_key(
        &json!({ "aws_account_id": account_id, "region": aws.region, "identity": t.identity }),
    );
    if identity_created {
        note_owned(state, &setup_id, true, &[]);
        save_state(st, site_id, run_id, "running", state).await?;
    }
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
    // An identity that already existed may have DKIM off or not configured: fix that, since the
    // DNS records alone don't turn signing on. Bring-your-own-DKIM identities are left alone.
    let dk = &info["DkimAttributes"];
    if dk["SigningAttributesOrigin"]
        .as_str()
        .is_some_and(|o| o.starts_with("EXTERNAL"))
    {
        anyhow::bail!("{} uses its own DKIM keys (BYODKIM); configure its DKIM DNS yourself, or switch it to Easy DKIM in SES and re-run", t.identity);
    }
    let mut info = info;
    if tokens(&info).is_empty() {
        aws.ses(
            "PUT",
            &format!("/v2/email/identities/{}/dkim/signing", t.identity),
            Some(json!({ "SigningAttributesOrigin": "AWS_SES" })),
        )
        .await?;
        info = identity_info(aws, &t.identity)
            .await?
            .ok_or_else(|| anyhow::anyhow!("identity disappeared"))?;
    }
    if info["DkimAttributes"]["SigningEnabled"].as_bool() == Some(false) {
        aws.ses(
            "PUT",
            &format!("/v2/email/identities/{}/dkim", t.identity),
            Some(json!({ "SigningEnabled": true })),
        )
        .await?;
    }
    let dkim = tokens(&info);
    step(
        st,
        site_id,
        run_id,
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
    step(st, site_id, run_id, state, "mail_from", "running", "").await?;
    aws.ses(
        "PUT",
        &format!("/v2/email/identities/{}/mail-from", t.identity),
        Some(json!({ "MailFromDomain": t.mail_from, "BehaviorOnMxFailure": "USE_DEFAULT_VALUE" })),
    )
    .await?;
    step(
        st,
        site_id,
        run_id,
        state,
        "mail_from",
        "done",
        t.mail_from.clone(),
    )
    .await?;

    // 4. DNS records: create, never edit; delete only another region's SES MAIL FROM MX.
    let mut wanted: Vec<Record> = wanted_records(
        t,
        &aws.region,
        &dkim_domain(&aws.region, Some(&info)),
        &dkim,
    )
    .into_iter()
    .map(|(_, r)| r)
    .collect();
    let mut dkim_created = Vec::new();
    match dns {
        Some(dns) => {
            step(st, site_id, run_id, state, "dns", "running", "").await?;
            let existing = dns.list(&t.zone).await?;
            if !dmarc_present(&existing, t) {
                wanted.push(dmarc_record());
            }
            let created_ids = created_record_ids(state);
            let shared = sites_sharing(st, t.site.account_id, site_id, &t.identity)
                .await
                .map_err(|e| anyhow::anyhow!("{}", e.message))?;
            let mut created = Vec::new();
            let mut replaced = Vec::new();
            for rec in &wanted {
                let (action, note, stale, create) =
                    record_plan(rec, &existing, &aws.region, &created_ids, &shared);
                if action == "conflict" {
                    anyhow::bail!(
                        "{} {}: {}",
                        rec.kind,
                        rec.fqdn(&t.zone),
                        note.unwrap_or_default()
                    );
                }
                // New record first, then the old region's: a failure in between never leaves the
                // MAIL FROM domain without an MX (a re-run removes the leftover one).
                if create {
                    let id = dns.create(&t.zone, rec).await?;
                    let r = json!({ "id": id, "fqdn": rec.fqdn(&t.zone), "type": rec.kind, "content": rec.content });
                    // Recorded right away: if a later step fails, the run's final save keeps it.
                    merge_records(&mut state["resources"]["dns_records"], &json!([r.clone()]));
                    note_owned(
                        state,
                        &setup_id,
                        false,
                        &dkim_records_of(&json!([r.clone()]), &t.identity),
                    );
                    created.push(r);
                }
                for old in stale {
                    dns.delete(&t.zone, &old.id).await?;
                    replaced.push(format!("{} {}", old.kind, old.content));
                }
            }
            let n = created.len();
            dkim_created = dkim_records_of(&Value::Array(created.clone()), &t.identity);
            step(
                st,
                site_id,
                run_id,
                state,
                "dns",
                "done",
                if replaced.is_empty() {
                    format!("{n} record(s) created")
                } else {
                    format!("{n} record(s) created; replaced {}", replaced.join(", "))
                },
            )
            .await?;
        }
        None => {
            step(
                st,
                site_id,
                run_id,
                state,
                "dns",
                "manual",
                "add the records shown in the plan at your DNS provider",
            )
            .await?;
        }
    }

    // 5. Send-only IAM user; its key replaces the site's SES credentials.
    step(st, site_id, run_id, state, "sender", "running", "").await?;
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
    let send_policy = send_policy(&aws.region, &account_id, &t.identity, &t.config_set);
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

    // Reuse the key this setup already created for this IAM user in this AWS account (kept,
    // encrypted, in the run state); otherwise mint one and record it *immediately*, so an
    // interruption can't strand keys and use up the user's two key slots.
    // Keys are kept per AWS account + IAM user, so switching accounts back and forth reuses them.
    let key_slot = format!("{account_id}/{}", t.iam_user);
    let saved = state["resources"]["sender_keys"][&key_slot].clone();
    // Only reuse it if AWS still has it as an active key of this user (it may have been deleted or
    // deactivated, or the user recreated); otherwise mint a replacement.
    let reusable = match saved["access_key_id"].as_str().filter(|k| !k.is_empty()) {
        Some(saved_id) => {
            let xml = aws
                .query(
                    "iam",
                    &[("Action", "ListAccessKeys"), ("UserName", &t.iam_user)],
                )
                .await?;
            active_keys(&xml).iter().any(|k| k == saved_id)
        }
        None => false,
    };
    let (key_id, secret) = if reusable {
        (
            saved["access_key_id"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            crate::crypto::decrypt(saved["secret_access_key"].as_str().unwrap_or_default())?,
        )
    } else {
        let xml = aws.query("iam", &[("Action", "CreateAccessKey"), ("UserName", &t.iam_user)]).await.map_err(|e| {
            if e.code == "LimitExceeded" {
                anyhow::anyhow!("{e} — delete an unused access key of IAM user {} in the AWS console, then re-run", t.iam_user)
            } else {
                e.into()
            }
        })?;
        let key_id =
            xml_tag(&xml, "AccessKeyId").ok_or_else(|| anyhow::anyhow!("no AccessKeyId"))?;
        let secret = xml_tag(&xml, "SecretAccessKey")
            .ok_or_else(|| anyhow::anyhow!("no SecretAccessKey"))?;
        if !state["resources"]["sender_keys"].is_object() {
            state["resources"]["sender_keys"] = json!({});
        }
        state["resources"]["sender_keys"][&key_slot] = json!({
            "iam_user": t.iam_user, "aws_account_id": account_id,
            "access_key_id": key_id, "secret_access_key": crate::crypto::encrypt(&secret)?,
        });
        save_state(st, site_id, run_id, "running", state).await?;
        (key_id, secret)
    };
    let mut cfg = json!({
        "region": aws.region,
        "access_key_id": key_id,
        "secret_access_key": secret,
        "configuration_set": t.config_set,
    });
    crate::crypto::seal_config(&mut cfg)?;
    // The site's new credentials and the record of what it now sends through are saved in one
    // transaction, so the two can never disagree (e.g. after a crash in between).
    let mut after = state.clone();
    activate(
        &mut after,
        json!({
            "aws_account_id": account_id, "region": aws.region, "identity": t.identity, "zone": t.zone,
            "configuration_set": t.config_set, "topic_arn": state["resources"]["topic_arn"].clone(),
            "iam_user": t.iam_user, "access_key_id": key_id,
            "identity_created": identity_created, "dns_records": dkim_created,
        }),
    );
    after["resources"]["iam_user"] = t.iam_user.clone().into();
    after["resources"]["access_key_id"] = key_id.clone().into();
    after["resources"]["aws_account_id"] = account_id.clone().into();
    switch_site_provider(&st.db, &t.site, run_id, &cfg, &after).await?;
    *state = after;
    step(
        st,
        site_id,
        run_id,
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

#[derive(Debug, PartialEq)]
enum SubState {
    Missing,
    Pending,
    Confirmed,
}

/// Our webhook's subscription state in a `ListSubscriptionsByTopic` response. Pending ones have
/// `SubscriptionArn` = `PendingConfirmation`.
fn subscription_state(xml: &str, endpoint: &str) -> SubState {
    let escaped = endpoint.replace('&', "&amp;");
    let mut state = SubState::Missing;
    for m in xml.split("<member>").skip(1) {
        let ep = xml_tag(m, "Endpoint").unwrap_or_default();
        if ep != endpoint && ep != escaped {
            continue;
        }
        match xml_tag(m, "SubscriptionArn").as_deref() {
            Some(arn) if arn.starts_with("arn:") => return SubState::Confirmed,
            _ => state = SubState::Pending,
        }
    }
    state
}

/// Active access key ids from an IAM `ListAccessKeys` response.
fn active_keys(xml: &str) -> Vec<String> {
    xml.split("<member>")
        .skip(1)
        .filter(|m| xml_tag(m, "Status").as_deref() == Some("Active"))
        .filter_map(|m| xml_tag(m, "AccessKeyId"))
        .collect()
}

/// Points the site at the new SES credentials, as one transaction that first re-asserts this run
/// still owns the setup (a row lock that a takeover's claim must wait for). Also refuses if the
/// site's sender or provider settings changed since setup started, so newer credentials are never
/// replaced with a key scoped to an old domain.
async fn switch_site_provider(
    db: &crate::db::Db,
    site: &Site,
    run_id: &str,
    cfg: &Value,
    state: &Value,
) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    let owned = sqlx::query(
        "UPDATE site_setups SET updated_at = $1, state = $4 WHERE site_id = $2 AND run_id = $3",
    )
    .bind(now())
    .bind(site.id)
    .bind(run_id)
    .bind(state.to_string())
    .execute(&mut *tx)
    .await?;
    if owned.rows_affected() == 0 {
        anyhow::bail!("this setup run was superseded by a newer one");
    }
    let switched = sqlx::query(
        "UPDATE sites SET provider_kind = 'ses', provider_config = $1 WHERE id = $2 \
         AND from_email = $3 AND domain = $4 AND provider_kind = $5 AND provider_config = $6",
    )
    .bind(cfg.to_string())
    .bind(site.id)
    .bind(&site.from_email)
    .bind(&site.domain)
    .bind(&site.provider_kind)
    .bind(&site.provider_config)
    .execute(&mut *tx)
    .await?;
    if switched.rows_affected() == 0 {
        anyhow::bail!("the site's sender or email provider settings changed while setup was running; review them and run setup again (the new key is kept and will be reused)");
    }
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Status & verification

/// A running setup that hasn't saved progress for this long is treated as abandoned (the server
/// running it stopped). Each step saves progress, which renews the lease.
const RUN_LEASE_MINUTES: i64 = 5;

pub(crate) fn lease_cutoff() -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(RUN_LEASE_MINUTES))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Atomically claims the site's setup for a new run (one at a time, across server processes).
async fn claim_run(st: &AppState, site_id: i64, run_id: &str) -> AppResult<bool> {
    // Existing state is left untouched; only status/ownership change.
    let r = sqlx::query(
        "INSERT INTO site_setups (site_id, status, state, run_id, updated_at) VALUES ($1, 'running', '{}', $2, $3) \
         ON CONFLICT (site_id) DO UPDATE SET status = 'running', run_id = excluded.run_id, updated_at = excluded.updated_at \
         WHERE site_setups.status <> 'running' OR site_setups.updated_at < $4",
    )
    .bind(site_id)
    .bind(run_id)
    .bind(now())
    .bind(lease_cutoff())
    .execute(&st.db)
    .await?;
    Ok(r.rows_affected() == 1)
}

async fn load_state(st: &AppState, site_id: i64) -> AppResult<Option<(String, Value, String)>> {
    let row: Option<(String, String, String)> =
        sqlx::query_as("SELECT status, state, updated_at FROM site_setups WHERE site_id = $1")
            .bind(site_id)
            .fetch_optional(&st.db)
            .await?;
    Ok(row.map(|(s, j, u)| (s, serde_json::from_str(&j).unwrap_or_default(), u)))
}

/// Saves progress for the run that owns the site's setup. Fails if another run took over
/// (fencing), which stops a superseded run from writing anything further.
async fn save_state(
    st: &AppState,
    site_id: i64,
    run_id: &str,
    status: &str,
    state: &Value,
) -> anyhow::Result<()> {
    let r = sqlx::query("UPDATE site_setups SET status = $1, state = $2, updated_at = $3 WHERE site_id = $4 AND run_id = $5")
        .bind(status)
        .bind(state.to_string())
        .bind(now())
        .bind(site_id)
        .bind(run_id)
        .execute(&st.db)
        .await?;
    if r.rows_affected() == 0 {
        anyhow::bail!("this setup run was superseded by a newer one");
    }
    Ok(())
}

/// Setups the site moved away from that may still have resources, with how to clean them up.
/// One the site still sends with (its key) is never listed.
async fn previous_setups(st: &AppState, site_id: i64, state: &Value) -> AppResult<Vec<Value>> {
    let Some(map) = state["previous"].as_object().filter(|m| !m.is_empty()) else {
        return Ok(Vec::new());
    };
    let (account_id,): (i64,) = sqlx::query_as("SELECT account_id FROM sites WHERE id = $1")
        .bind(site_id)
        .fetch_one(&st.db)
        .await?;
    let current = sending_config(st, site_id).await?;
    // Every other setup of this site, to tell which per-site resources are still shared.
    let others = |skip: &str| -> Vec<&Value> {
        std::iter::once(&state["active"])
            .filter(|a| a.is_object())
            .chain(
                map.iter()
                    .filter(|(k, _)| k.as_str() != skip)
                    .map(|(_, v)| v),
            )
            .collect()
    };
    let mut out = Vec::new();
    for (id, p) in map {
        if in_use(p, current.as_ref()) {
            continue;
        }
        let identity = p["identity"].as_str().unwrap_or_default();
        let shared = sites_sharing(st, account_id, site_id, identity).await?;
        out.push(json!({
            "id": id, "region": p["region"], "identity": identity, "aws_account_id": p["aws_account_id"],
            "dns_records": p["dns_records"], "dns_removable": identity_removable(p, &shared),
            "shared_with": shared, "commands": cleanup_commands(p, &others(id), &shared),
        }));
    }
    Ok(out)
}

async fn status_json(
    st: &AppState,
    site_id: i64,
    verify: bool,
    aws: Option<&Aws>,
) -> AppResult<Value> {
    let Some((mut status, mut state, updated_at)) = load_state(st, site_id).await? else {
        return Ok(json!({ "status": "not_started" }));
    };
    // No progress within the lease: the server running it stopped. Report it as retryable.
    if status == "running" && updated_at < lease_cutoff() {
        status = "error".into();
        state["error"] = "interrupted (the server stopped mid-run); run setup again".into();
    }
    // Saved send-only keys stay server-side (even encrypted).
    if let Some(keys) = state["resources"]["sender_keys"].as_object_mut() {
        for k in keys.values_mut() {
            if let Some(o) = k.as_object_mut() {
                o.remove("secret_access_key");
            }
        }
    }
    let previous = previous_setups(st, site_id, &state).await?;
    let mut out = json!({ "status": status, "state": state, "previous_regions": previous });
    if verify {
        if let (Some(aws), Some(identity)) = (aws, state["plan"]["identity"].as_str()) {
            // A failed live check (e.g. the setup key was deleted, as recommended) must not hide
            // the saved run; report it separately.
            match identity_info(aws, identity).await {
                Ok(Some(info)) => {
                    out["verification"] = json!({
                        "dkim": info["DkimAttributes"]["Status"],
                        "mail_from": info["MailFromAttributes"]["MailFromDomainStatus"],
                        "verified_for_sending": info["VerifiedForSendingStatus"],
                    });
                }
                Ok(None) => {
                    out["verification_error"] = format!("{identity} no longer exists in SES").into()
                }
                Err(e) => {
                    out["verification_error"] = format!("couldn't check verification: {e}").into()
                }
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
                .and_then(|(_, s, _)| s["plan"]["region"].as_str().map(str::to_string));
            // A removed integration shouldn't hide the saved run: report it as a verification error.
            match integrations::load(&st, &user, i)
                .await
                .and_then(|int| int.aws(&st.http, region.as_deref()))
            {
                Ok(aws) => Some(aws),
                Err(e) => {
                    let mut out = status_json(&st, id, false, None).await?;
                    out["verification_error"] =
                        format!("couldn't check verification: {}", e.message).into();
                    return Ok(Json(out));
                }
            }
        }
        None => None,
    };
    status_json(&st, id, aws.is_some(), aws.as_ref())
        .await
        .map(Json)
}

// ---------------------------------------------------------------------------------------
// Cleaning up a region setup moved away from

#[derive(Deserialize)]
pub struct CleanupRequest {
    /// The previous setup's `id` from `previous_regions` in the status.
    pub id: String,
    /// Delete the setup's DNS records (that setup created) through this DNS integration.
    #[serde(default)]
    pub dns_integration_id: Option<i64>,
    /// Stop listing the setup (after its AWS resources were removed).
    #[serde(default)]
    pub forget: bool,
}

/// Cleans up a setup the site moved away from: deletes the DNS records setup created for it
/// (only ones that still exist unchanged, and never for an identity setup didn't create or another
/// site uses), and/or forgets it. AWS resources are listed as commands for the user to run: the
/// setup key isn't allowed to delete anything. Holds the site's setup lock throughout, so it can't
/// overlap a setup run or another cleanup.
pub async fn cleanup(
    State(st): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<CleanupRequest>,
) -> AppResult<Json<Value>> {
    let site = load_site(&st, &user, id).await?;
    let Some((prev_status, _, prev_updated)) = load_state(&st, id).await? else {
        return Err(AppError::not_found());
    };
    let busy = || {
        AppError::new(
            axum::http::StatusCode::CONFLICT,
            "setup is running for this site; try again when it finishes",
        )
    };
    // Claim the site's setup lock, but only from exactly the state just read, so the status
    // restored afterwards is the one it replaced. A run abandoned mid-way is restored as an error.
    let run_id = crate::db::random_token(12);
    let claimed = sqlx::query(
        "UPDATE site_setups SET status = 'running', run_id = $1, updated_at = $2 \
         WHERE site_id = $3 AND status = $4 AND updated_at = $5 AND (status <> 'running' OR updated_at < $6)",
    )
    .bind(&run_id)
    .bind(now())
    .bind(id)
    .bind(&prev_status)
    .bind(&prev_updated)
    .bind(lease_cutoff())
    .execute(&st.db)
    .await?;
    if claimed.rows_affected() == 0 {
        return Err(busy());
    }
    let restore_status = if prev_status == "running" {
        "error"
    } else {
        prev_status.as_str()
    };
    let result = cleanup_locked(&st, &user, &site, &req).await;
    // Release the lock, restoring the status setup had (with the updated state on success).
    let (mut state, out) = match result {
        Ok((state, out)) => (state, Ok(out)),
        Err(e) => (
            load_state(&st, id)
                .await?
                .map(|(_, s, _)| s)
                .unwrap_or_default(),
            Err(e),
        ),
    };
    if prev_status == "running" && state["error"].is_null() {
        state["error"] = "interrupted (the server stopped mid-run); run setup again".into();
    }
    save_state(&st, id, &run_id, restore_status, &state)
        .await
        .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    out.map(Json)
}

async fn cleanup_locked(
    st: &AppState,
    user: &AuthUser,
    site: &Site,
    req: &CleanupRequest,
) -> AppResult<(Value, Value)> {
    let mut state = load_state(st, site.id)
        .await?
        .map(|(_, s, _)| s)
        .unwrap_or_default();
    let entry = state["previous"][&req.id].clone();
    if !entry.is_object() {
        return Err(AppError::not_found());
    }
    if in_use(&entry, sending_config(st, site.id).await?.as_ref()) {
        return Err(AppError::bad_request(format!(
            "{} still sends through {}; move it to another region first",
            site.name,
            entry["region"].as_str().unwrap_or("this region")
        )));
    }
    let mut deleted = Vec::new();
    if let Some(dns_id) = req.dns_integration_id {
        let identity = entry["identity"].as_str().unwrap_or_default();
        let shared = sites_sharing(st, user.account_id, site.id, identity).await?;
        if !identity_removable(&entry, &shared) {
            return Err(AppError::bad_request(format!(
                "{identity}'s DKIM records are still needed (another site sends from it, or setup didn't create it)"
            )));
        }
        let dns = integrations::load(st, user, dns_id).await?.dns(&st.http)?;
        // The zone the records were created in (the site's domain may have changed since).
        let zone = entry["zone"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| site.domain.trim().to_ascii_lowercase());
        let existing = dns
            .list(&zone)
            .await
            .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
        let mut kept = Vec::new();
        for r in entry["dns_records"].as_array().into_iter().flatten() {
            let (Some(rid), Some(fqdn), Some(kind), Some(content)) = (
                r["id"].as_str().filter(|s| !s.is_empty()),
                r["fqdn"].as_str(),
                r["type"].as_str(),
                r["content"].as_str(),
            ) else {
                kept.push(r.clone());
                continue;
            };
            // Only the exact record setup created: same id, name, type and value.
            let unchanged = existing.iter().any(|e| {
                e.id == rid
                    && e.kind == kind
                    && e.fqdn(&zone).eq_ignore_ascii_case(fqdn)
                    && same_content(kind, &e.content, content)
            });
            if unchanged {
                dns.delete(&zone, rid)
                    .await
                    .map_err(|e| AppError::bad_request(format!("deleting {fqdn}: {e:#}")))?;
                deleted.push(fqdn.to_string());
            }
            // Gone or changed since: nothing of setup's left to delete.
        }
        state["previous"][&req.id]["dns_records"] = Value::Array(kept);
    }
    if req.forget {
        if let Some(m) = state["previous"].as_object_mut() {
            m.remove(&req.id);
        }
    }
    Ok((state, json!({ "deleted_dns_records": deleted })))
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
    let contacts: Vec<String> = p
        .contact_emails
        .iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect();
    if contacts.len() > 4 {
        return Err(AppError::bad_request(
            "AWS accepts at most 4 contact emails",
        ));
    }
    if let Some(bad) = contacts.iter().find(|e| !crate::public::valid_email(e)) {
        return Err(AppError::bad_request(format!(
            "{bad} is not a valid email address"
        )));
    }
    let mut details = json!({
        "MailType": p.mail_type,
        "WebsiteURL": p.website_url,
        "UseCaseDescription": p.use_case_description,
        "ContactLanguage": "EN",
        "ProductionAccessEnabled": true,
    });
    // Optional, but AWS rejects an empty list: only send it when there are addresses.
    if !contacts.is_empty() {
        details["AdditionalContactEmailAddresses"] = json!(contacts);
    }
    let aws = integrations::load(&st, &user, p.aws_integration_id)
        .await?
        .aws(&st.http, p.region.as_deref())?;
    aws.ses("POST", "/v2/email/account/details", Some(details))
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

    #[test]
    fn send_policy_allows_recipient_identities_but_pins_the_sender() {
        let p: Value = serde_json::from_str(&send_policy(
            "us-west-1",
            "123",
            "news.example.com",
            "lc-cs",
        ))
        .unwrap();
        let st = &p["Statement"][0];
        assert_eq!(
            st["Resource"],
            json!([
                "arn:aws:ses:us-west-1:123:identity/*",
                "arn:aws:ses:us-west-1:123:configuration-set/lc-cs"
            ])
        );
        assert_eq!(
            st["Condition"]["StringLike"]["ses:FromAddress"],
            "*@news.example.com"
        );
    }

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

    fn mx(name: &str, content: &str, prio: u16) -> Record {
        Record {
            priority: Some(prio),
            ..rec(name, "MX", content)
        }
    }

    #[test]
    fn another_regions_mail_from_mx_is_replaced() {
        let want = mx("bounce.news", "feedback-smtp.us-east-1.amazonses.com", 10);
        let old = mx("bounce.news", "feedback-smtp.us-west-1.amazonses.com.", 10);
        let ours = vec![old.id.clone()];
        let plan = |existing: &[Record], ids: &[String], shared: &[String]| {
            let (a, n, stale, create) = record_plan(&want, existing, "us-east-1", ids, shared);
            (a, n, stale.len(), create)
        };

        let (action, note, stale, create) = plan(std::slice::from_ref(&old), &ours, &[]);
        assert_eq!((action, stale, create), ("replace", 1, true));
        assert!(note.unwrap().contains("us-west-1"));

        // Both regions' records present: remove the stale one, nothing to create.
        let both = vec![old.clone(), want.clone()];
        assert_eq!(
            plan(&both, &ours, &[]),
            ("replace", plan(&both, &ours, &[]).1, 1, false)
        );

        // Not created by setup, or another site sends from that domain: never touched.
        let (action, note, ..) = plan(std::slice::from_ref(&old), &[], &[]);
        assert_eq!(action, "conflict");
        assert!(note.unwrap().contains("wasn't created by Listaculous"));
        let (action, note, ..) = plan(std::slice::from_ref(&old), &ours, &["Other".to_string()]);
        assert_eq!(action, "conflict");
        assert!(note.unwrap().contains("Other"));

        // Any other MX at that name still blocks, even next to a stale SES one.
        let mixed = vec![old.clone(), mx("bounce.news", "mx.example.net", 20)];
        assert_eq!(plan(&mixed, &ours, &[]).0, "conflict");
        let fake = vec![mx("bounce.news", "feedback-smtp.evil.example.com", 10)];
        assert_eq!(plan(&fake, &ours, &[]).0, "conflict");

        // Same region, or a different name: never stale.
        assert!(stale_ses_mx(&want, std::slice::from_ref(&want), "us-east-1").is_empty());
        let elsewhere = mx("bounce.other", "feedback-smtp.us-west-1.amazonses.com", 10);
        assert!(stale_ses_mx(&want, &[elsewhere], "us-east-1").is_empty());
        // Only MX records are ever considered.
        let txt = rec("bounce.news", "TXT", "v=spf1 include:amazonses.com ~all");
        assert_eq!(
            record_plan(&txt, &[old], "us-east-1", &ours, &[]).2.len(),
            0
        );
    }

    fn setup_rec(region: &str, key: &str, created: Value, dkim: &[&str]) -> Value {
        json!({
            "aws_account_id": "123", "region": region, "identity": "news.example.com",
            "configuration_set": "cs", "topic_arn": format!("arn:aws:sns:{region}:123:t"),
            "iam_user": format!("u-{region}"), "access_key_id": key, "identity_created": created,
            "dns_records": dkim.iter().map(|id| json!({ "id": id, "fqdn": format!("{id}._domainkey.news.example.com"),
                "type": "CNAME", "content": "x.dkim.amazonses.com" })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn region_moves_keep_track_of_previous_setups() {
        let mut state = json!({});
        activate(
            &mut state,
            setup_rec("us-west-1", "AKIAW", json!(true), &["w1"]),
        );
        assert!(state["previous"].is_null(), "first setup: nothing previous");

        // Re-running the same setup keeps what it knew (records, created-ness).
        activate(
            &mut state,
            setup_rec("us-west-1", "AKIAW", json!(false), &[]),
        );
        assert_eq!(state["active"]["dns_records"].as_array().unwrap().len(), 1);
        assert_eq!(state["active"]["identity_created"], true);

        // Moving: the old setup becomes previous, keyed by account/region/identity.
        activate(
            &mut state,
            setup_rec("us-east-1", "AKIAE", json!(true), &["e1"]),
        );
        let prev = &state["previous"]["123/us-west-1/news.example.com"];
        assert_eq!(prev["access_key_id"], "AKIAW");
        assert_eq!(state["active"]["region"], "us-east-1");

        // Moving back takes the previous record (and its DNS ids) back, and archives the other.
        activate(
            &mut state,
            setup_rec("us-west-1", "AKIAW", json!(false), &[]),
        );
        assert!(state["previous"]["123/us-west-1/news.example.com"].is_null());
        assert!(state["previous"]["123/us-east-1/news.example.com"].is_object());
        assert_eq!(state["active"]["dns_records"][0]["id"], "w1");
        assert_eq!(state["active"]["identity_created"], true);

        // Same region in another AWS account is a different setup.
        let mut other = setup_rec("us-east-1", "AKIAO", json!(true), &[]);
        other["aws_account_id"] = "999".into();
        activate(&mut state, other);
        assert!(state["previous"]["123/us-east-1/news.example.com"].is_object());
        assert!(state["previous"]["123/us-west-1/news.example.com"].is_object());
    }

    #[test]
    fn legacy_setups_are_adopted_only_while_sending() {
        let mut site = Site {
            id: 1,
            account_id: 1,
            name: "S".into(),
            domain: "example.com".into(),
            from_name: "S".into(),
            from_email: "hi@news.example.com".into(),
            reply_to: String::new(),
            physical_address: String::new(),
            provider_kind: "ses".into(),
            provider_config: json!({ "region": "us-west-1", "access_key_id": "AKIAW" }).to_string(),
            send_rate: 1,
            webhook_secret: "w".into(),
            created_at: String::new(),
            icon_type: None,
            icon_checked_at: None,
        };
        let legacy = json!({
            "plan": { "identity": "news.example.com" },
            "resources": { "aws_account_id": "123", "iam_user": "u", "access_key_id": "AKIAW",
                "dns_records": [{ "id": "1", "fqdn": "t._domainkey.news.example.com", "type": "CNAME", "content": "c" },
                                { "id": "2", "fqdn": "bounce.news.example.com", "type": "TXT", "content": "spf" }] },
        });
        let mut state = legacy.clone();
        adopt_legacy_active(&mut state, &site);
        assert_eq!(state["active"]["region"], "us-west-1");
        assert_eq!(
            state["active"]["dns_records"].as_array().unwrap().len(),
            1,
            "DKIM only"
        );
        assert!(state["active"]["identity_created"].is_null(), "unknown");

        // The site moved on to another key (e.g. a failed later run didn't switch): don't adopt.
        site.provider_config =
            json!({ "region": "us-west-1", "access_key_id": "AKIAX" }).to_string();
        let mut state = legacy;
        adopt_legacy_active(&mut state, &site);
        assert!(state["active"].is_null());
    }

    #[test]
    fn cleanup_spares_identities_setup_did_not_create_or_others_use() {
        let elsewhere = setup_rec("us-east-1", "AKIAE", json!(true), &[]);
        let mine = setup_rec("us-west-1", "AKIAW", json!(true), &["w1"]);
        let cmds = cleanup_commands(&mine, &[&elsewhere], &[]);
        assert_eq!(cmds.first().unwrap(), "(set -e");
        assert!(
            cmds[1].contains("get-caller-identity") && cmds[1].contains("\"123\""),
            "checks the account first"
        );
        assert_eq!(cmds.last().unwrap(), ")");
        let all = cmds.join("\n");
        assert!(all.contains(
            "delete-email-identity --email-identity news.example.com --region us-west-1"
        ));
        assert!(all.contains("delete-access-key --user-name u-us-west-1 --access-key-id AKIAW"));
        assert!(
            all.contains("delete-topic --topic-arn arn:aws:sns:us-west-1:123:t --region us-west-1")
        );
        assert!(
            all.contains("delete-configuration-set --configuration-set-name cs --region us-west-1")
        );
        assert!(identity_removable(&mine, &[]));

        // Another *previous* setup in the same account and region also keeps them (for the last).
        let sibling = setup_rec("us-west-1", "AKIAB", json!(true), &[]);
        let all = cleanup_commands(&mine, &[&elsewhere, &sibling], &[]).join("\n");
        assert!(!all.contains("delete-configuration-set") && !all.contains("delete-topic"));
        assert!(
            all.contains("delete-email-identity"),
            "the identity itself can still go"
        );
        // Same account and region, other identity: the per-site config set and topic are shared.
        let same_place = setup_rec("us-west-1", "AKIAN", json!(true), &[]);
        let all = cleanup_commands(&mine, &[&same_place], &[]).join("\n");
        assert!(!all.contains("delete-configuration-set") && !all.contains("delete-topic"));
        assert!(all.contains("delete-user --user-name u-us-west-1"));

        let found = setup_rec("us-west-1", "AKIAW", json!(false), &[]);
        assert!(!cleanup_commands(&found, &[&elsewhere], &[])
            .join("\n")
            .contains("delete-email-identity"));
        assert!(!identity_removable(&found, &[]));

        let shared = vec!["Other site".to_string()];
        assert!(!cleanup_commands(&mine, &[&elsewhere], &shared)
            .join("\n")
            .contains("delete-email-identity"));
        assert!(!identity_removable(&mine, &shared));
        // A kept identity keeps its default configuration set (and topic) too.
        let kept = cleanup_commands(&mine, &[&elsewhere], &shared).join("\n");
        assert!(!kept.contains("delete-configuration-set") && !kept.contains("delete-topic"));
        assert!(kept.contains("# Kept: news.example.com"));
        assert!(
            kept.contains("delete-user --user-name u-us-west-1"),
            "the old key still goes"
        );

        // Unknown (old setups): the identity deletion is left commented out.
        let unknown = setup_rec("us-west-1", "AKIAW", Value::Null, &[]);
        let all = cleanup_commands(&unknown, &[&elsewhere], &[]).join("\n");
        assert!(all.contains("\n# del aws sesv2 delete-email-identity"));
        assert!(all.contains("\n# del aws sesv2 delete-configuration-set"));
        // The key goes first; resources others may have removed don't stop the block.
        let mine_cmds = cleanup_commands(&mine, &[&elsewhere], &[]);
        assert!(mine_cmds[2].starts_with("del() {"));
        assert!(mine_cmds[3].contains("delete-access-key"));
        assert!(mine_cmds
            .iter()
            .any(|c| c.starts_with("del aws sesv2 delete-configuration-set")));
    }

    #[test]
    fn replacement_records_keep_their_own_ids() {
        // west MX (id 1) is replaced by east MX (id 2) at the same name: both ids are kept, so a
        // later move back can recognise the east record as setup's.
        let mut all =
            json!([{ "id": "1", "fqdn": "bounce.news.example.com", "type": "MX", "content": "w" }]);
        merge_records(
            &mut all,
            &json!([{ "id": "2", "fqdn": "bounce.news.example.com", "type": "MX", "content": "e" }]),
        );
        merge_records(
            &mut all,
            &json!([{ "id": "2", "fqdn": "bounce.news.example.com", "type": "MX", "content": "e" }]),
        );
        let ids: Vec<&str> = all
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["1", "2"]);
        // Without ids: by name and type.
        let mut manual = json!([{ "fqdn": "a", "type": "CNAME" }]);
        merge_records(&mut manual, &json!([{ "fqdn": "a", "type": "CNAME" }]));
        assert_eq!(manual.as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_previous_setup_the_site_uses_again_is_protected() {
        let west = setup_rec("us-west-1", "AKIAW", json!(true), &[]);
        assert!(in_use(
            &west,
            Some(&json!({ "region": "us-west-1", "access_key_id": "AKIAW" }))
        ));
        // Switched back by hand with a different key: still the same region, still protected.
        assert!(in_use(
            &west,
            Some(&json!({ "region": "us-west-1", "access_key_id": "AKIANEW" }))
        ));
        assert!(!in_use(
            &west,
            Some(&json!({ "region": "us-east-1", "access_key_id": "AKIAE" }))
        ));
        assert!(!in_use(&west, None), "not sending with SES at all");
    }

    #[tokio::test]
    async fn provider_changes_wait_for_setup_and_cleanup() {
        let path = std::env::temp_dir().join(format!("lc-setup-{}.db", crate::db::random_token(8)));
        let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query("INSERT INTO accounts (name, created_at) VALUES ('a', 'now')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sites (account_id, name, domain, from_name, from_email, webhook_secret, created_at) \
                     VALUES (1, 'S', 'example.com', 'F', 'hi@news.example.com', 'w', 'now')").execute(&db).await.unwrap();
        let update = |updated_at: String| {
            let db = db.clone();
            async move {
                sqlx::query("DELETE FROM site_setups")
                    .execute(&db)
                    .await
                    .unwrap();
                sqlx::query("INSERT INTO site_setups (site_id, status, state, run_id, updated_at) VALUES (1, 'running', '{}', 'r', $1)")
                    .bind(updated_at).execute(&db).await.unwrap();
                sqlx::query(crate::api::UPDATE_SITE_SQL)
                    .bind("S")
                    .bind("example.com")
                    .bind("F")
                    .bind("hi@news.example.com")
                    .bind("")
                    .bind("")
                    .bind("ses")
                    .bind(json!({ "region": "us-west-1" }).to_string())
                    .bind(1i64)
                    .bind(1i64)
                    .bind(lease_cutoff())
                    .execute(&db)
                    .await
                    .unwrap()
                    .rows_affected()
            }
        };
        // Setup (or a cleanup) holds the lock: the provider can't change underneath it.
        assert_eq!(update(now()).await, 0);
        // An abandoned run (lease expired) doesn't block.
        assert_eq!(update("2000-01-01T00:00:00Z".into()).await, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn what_a_failed_run_created_is_kept() {
        let mut state = json!({});
        let key = "123/us-west-1/news.example.com";
        // A run creates the identity and a DKIM record, then fails before switching the site.
        let rec = json!({ "id": "9", "fqdn": "t._domainkey.news.example.com", "type": "CNAME", "content": "c" });
        note_owned(&mut state, key, true, &[]);
        note_owned(&mut state, key, false, &[rec]);
        assert!(state["active"].is_null());
        // The retry finds both already there, yet ownership carries over.
        activate(
            &mut state,
            setup_rec("us-west-1", "AKIAW", json!(false), &[]),
        );
        assert_eq!(state["active"]["identity_created"], true);
        assert_eq!(state["active"]["dns_records"][0]["id"], "9");
        assert!(state["owned"][key].is_null());
    }

    #[test]
    fn a_match_does_not_hide_conflicting_neighbours() {
        let mut ses_mx = rec("bounce.news", "MX", "feedback-smtp.us-east-1.amazonses.com");
        ses_mx.priority = Some(10);
        let mut other_mx = rec("bounce.news", "MX", "mx.other.example");
        other_mx.priority = Some(5);
        let want = ses_mx.clone();
        assert_eq!(record_state(&want, &[ses_mx.clone()]).0, "exists");
        assert_eq!(
            record_state(&want, &[ses_mx.clone(), other_mx]).0,
            "conflict",
            "a second MX breaks MAIL FROM"
        );
        let mut wrong_prio = ses_mx.clone();
        wrong_prio.priority = Some(20);
        assert_eq!(record_state(&want, &[wrong_prio]).0, "conflict");
        let spf = rec("bounce.news", "TXT", "v=spf1 include:amazonses.com ~all");
        let extra_spf = rec("bounce.news", "TXT", "v=spf1 -all");
        assert_eq!(
            record_state(&spf, &[spf.clone(), extra_spf]).0,
            "conflict",
            "two SPF records are invalid"
        );
    }

    #[test]
    fn event_destination_must_match() {
        let topic = "arn:aws:sns:us-east-1:1:t";
        let good = json!({ "EventDestinations": [{ "Name": EVENT_DESTINATION, "Enabled": true,
            "MatchingEventTypes": ["BOUNCE", "COMPLAINT"], "SnsDestination": { "TopicArn": topic } }] });
        assert!(destination_ok(&good, topic));
        let mut disabled = good.clone();
        disabled["EventDestinations"][0]["Enabled"] = false.into();
        assert!(!destination_ok(&disabled, topic));
        let mut no_complaints = good.clone();
        no_complaints["EventDestinations"][0]["MatchingEventTypes"] = json!(["BOUNCE"]);
        assert!(!destination_ok(&no_complaints, topic));
        assert!(!destination_ok(&good, "arn:aws:sns:us-east-1:1:other"));
    }

    #[tokio::test]
    async fn final_switch_is_fenced_by_run_and_site_settings() {
        let path = std::env::temp_dir().join(format!("lc-setup-{}.db", crate::db::random_token(8)));
        let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query("INSERT INTO accounts (name, created_at) VALUES ('a', 'now')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sites (account_id, name, domain, from_name, from_email, webhook_secret, created_at) \
                     VALUES (1, 'S', 'example.com', 'F', 'hi@news.example.com', 'w', 'now')").execute(&db).await.unwrap();
        let site: Site = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SITE_COLS} FROM sites WHERE id = 1"
        )))
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO site_setups (site_id, status, state, run_id, updated_at) VALUES (1, 'running', '{}', 'old', 'now')")
            .execute(&db).await.unwrap();
        let cfg = json!({ "region": "us-east-1" });

        // A newer run took over: the old run must not switch the site.
        sqlx::query("UPDATE site_setups SET run_id = 'new' WHERE site_id = 1")
            .execute(&db)
            .await
            .unwrap();
        let err = switch_site_provider(&db, &site, "old", &cfg, &json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("superseded"), "{err}");
        let (kind,): (String,) = sqlx::query_as("SELECT provider_kind FROM sites WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(kind, "log");

        // The owning run is refused if the site's settings changed meanwhile…
        sqlx::query("UPDATE sites SET provider_kind = 'smtp' WHERE id = 1")
            .execute(&db)
            .await
            .unwrap();
        assert!(switch_site_provider(&db, &site, "new", &cfg, &json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("changed"));
        // …and succeeds when they're unchanged.
        let site: Site = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SITE_COLS} FROM sites WHERE id = 1"
        )))
        .fetch_one(&db)
        .await
        .unwrap();
        switch_site_provider(&db, &site, "new", &cfg, &json!({}))
            .await
            .unwrap();
        let (kind,): (String,) = sqlx::query_as("SELECT provider_kind FROM sites WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(kind, "ses");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parses_active_access_keys() {
        let xml = "<ListAccessKeysResult><AccessKeyMetadata>\
            <member><UserName>u</UserName><AccessKeyId>AKIA1</AccessKeyId><Status>Inactive</Status></member>\
            <member><UserName>u</UserName><AccessKeyId>AKIA2</AccessKeyId><Status>Active</Status></member>\
            </AccessKeyMetadata></ListAccessKeysResult>";
        assert_eq!(active_keys(xml), vec!["AKIA2".to_string()]);
        assert!(
            active_keys("<ListAccessKeysResult><AccessKeyMetadata/></ListAccessKeysResult>")
                .is_empty()
        );
    }

    #[test]
    fn dkim_targets_follow_region() {
        assert_eq!(dkim_domain("us-east-1", None), "dkim.amazonses.com");
        assert_eq!(
            dkim_domain("eu-south-1", None),
            "dkim.eu-south-1.amazonses.com"
        );
        assert_eq!(
            dkim_domain("af-south-1", None),
            "dkim.af-south-1.amazonses.com"
        );
        let reported = json!({ "DkimAttributes": { "SigningHostedZone": "dkim.example-zone.amazonses.com." } });
        assert_eq!(
            dkim_domain("us-east-1", Some(&reported)),
            "dkim.example-zone.amazonses.com",
            "SES's reported zone wins"
        );
    }

    #[test]
    fn sns_subscription_states() {
        let ep = "https://lc.example.com/webhooks/ses/1/abc";
        let pending = format!("<Subscriptions><member><SubscriptionArn>PendingConfirmation</SubscriptionArn><Endpoint>{ep}</Endpoint></member></Subscriptions>");
        let confirmed = format!("<Subscriptions><member><SubscriptionArn>arn:aws:sns:us-east-1:1:t:uuid</SubscriptionArn><Endpoint>{ep}</Endpoint></member></Subscriptions>");
        let other = "<Subscriptions><member><SubscriptionArn>arn:aws:sns:x</SubscriptionArn><Endpoint>https://other</Endpoint></member></Subscriptions>";
        assert_eq!(subscription_state(&pending, ep), SubState::Pending);
        assert_eq!(subscription_state(&confirmed, ep), SubState::Confirmed);
        assert_eq!(subscription_state(other, ep), SubState::Missing);
    }

    #[test]
    fn relative_names() {
        assert_eq!(rel("bounce.news.example.com", "example.com"), "bounce.news");
        assert_eq!(rel("example.com", "example.com"), "");
    }
}
