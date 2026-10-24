//! Background campaign sender. Sends are persisted as `campaign_sends` rows so a restart
//! resumes where it left off.
//!
//! Row lifecycle: queued → sending (claimed) → sent | failed | skipped. A row is claimed before the
//! provider is called and its result is written as soon as that call returns. Only failures where
//! the provider definitely did not accept the message are retried; anything ambiguous (timeouts,
//! crashes mid-send, abandoned claims) is marked failed with an `unknown:` error and never resent
//! automatically, so recipients don't get duplicates.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use futures_util::future::join_all;

use crate::db::now;
use crate::mailer::{self, FailureKind, SendError};
use crate::models::{
    render_campaign, Campaign, List, Recipient, Site, CAMPAIGN_COLS, LIST_COLS, SITE_COLS,
};
use crate::AppState;

/// Attempts per recipient before a failure becomes terminal (until the campaign is resumed).
pub const MAX_ATTEMPTS: i64 = 3;
/// Base retry delay; attempt n waits n × this.
const RETRY_BACKOFF_SECS: i64 = 30;
/// Max messages rendered / in flight at once, independent of the per-second rate (bounds memory).
const MAX_IN_FLIGHT: usize = 8;
/// A claim older than this (provider call + DB write never finished) is considered abandoned.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(600);

/// Per-site token buckets: each site may make `send_rate` provider calls per second, shared by all
/// its campaigns. Tokens are taken at claim time, so the limit holds regardless of tick timing,
/// slow providers or errors.
#[derive(Default)]
struct RateBuckets {
    buckets: HashMap<i64, (f64, Instant)>,
}

impl RateBuckets {
    fn take(&mut self, site_id: i64, rate: i64, want: usize) -> usize {
        let rate = rate.clamp(1, 500) as f64;
        let now = Instant::now();
        let (tokens, last) = self.buckets.entry(site_id).or_insert((rate, now));
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * rate).min(rate);
        *last = now;
        let granted = (tokens.floor() as usize).min(want);
        *tokens -= granted as f64;
        granted
    }

    fn refund(&mut self, site_id: i64, n: usize) {
        if let Some((tokens, _)) = self.buckets.get_mut(&site_id) {
            *tokens += n as f64;
        }
    }
}

fn ts_after(d: chrono::Duration) -> String {
    (chrono::Utc::now() + d).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub async fn run(st: AppState) {
    // At startup nothing can be in flight, so every claim is abandoned.
    if let Err(e) = recover_abandoned(&st, Duration::ZERO).await {
        tracing::error!("worker recovery failed: {e:#}");
    }
    let mut ticks: u64 = 0;
    let mut buckets = RateBuckets::default();
    loop {
        let started = tokio::time::Instant::now();
        ticks += 1;
        if ticks % 300 == 0 {
            if let Err(e) = recover_abandoned(&st, CLAIM_TIMEOUT).await {
                tracing::error!("worker recovery failed: {e:#}");
            }
        }
        if let Err(e) = tick(&st, &mut buckets).await {
            tracing::error!("worker error: {e:#}");
        }
        // Rate limiting is done by the token buckets; the tick interval only sets granularity.
        tokio::time::sleep_until(started + Duration::from_millis(200)).await;
    }
}

async fn recover_abandoned(st: &AppState, older_than: Duration) -> anyhow::Result<()> {
    let cutoff = ts_after(-chrono::Duration::from_std(older_than)?);
    let r = sqlx::query(
        "UPDATE campaign_sends SET status = 'failed', error = 'unknown: send was interrupted; may or may not have been delivered' \
         WHERE status = 'sending' AND (next_attempt_at IS NULL OR next_attempt_at <= $1)",
    )
    .bind(cutoff)
    .execute(&st.db)
    .await?;
    if r.rows_affected() > 0 {
        tracing::warn!(
            "{} interrupted sends marked as delivery-unknown",
            r.rows_affected()
        );
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct QueuedSend {
    id: i64,
    attempts: i64,
    token: String,
    email: String,
    name: String,
}

async fn tick(st: &AppState, buckets: &mut RateBuckets) -> anyhow::Result<()> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT c.id, l.site_id FROM campaigns c JOIN lists l ON l.id = c.list_id WHERE c.status = 'sending' ORDER BY c.id",
    )
    .fetch_all(&st.db)
    .await?;
    // The rate budget is per site (i.e. per provider account), shared by all its campaigns.
    let mut by_site: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for (cid, site_id) in rows {
        by_site.entry(site_id).or_default().push(cid);
    }
    for (site_id, campaign_ids) in by_site {
        let site: Site = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SITE_COLS} FROM sites WHERE id = $1"
        )))
        .bind(site_id)
        .fetch_one(&st.db)
        .await?;
        for cid in campaign_ids {
            match process_campaign(st, &site, cid, buckets).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!("campaign {cid}: {e:#}");
                    pause(st, cid).await?;
                }
            }
        }
    }
    Ok(())
}

async fn claim_chunk<'a>(
    st: &AppState,
    campaign_id: i64,
    chunk: &'a [QueuedSend],
) -> anyhow::Result<Vec<&'a QueuedSend>> {
    let mut tx = st.db.begin().await?;
    let mut batch = Vec::with_capacity(chunk.len());
    for q in chunk {
        let r = sqlx::query(
            "UPDATE campaign_sends SET status = 'sending', attempts = attempts + 1, next_attempt_at = $1 \
             WHERE id = $2 AND status = 'queued' \
             AND EXISTS (SELECT 1 FROM campaigns WHERE id = $3 AND status = 'sending') \
             AND EXISTS (SELECT 1 FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id \
               WHERE s.id = campaign_sends.subscription_id AND s.token = $4 AND s.status = 'confirmed' AND sb.status = 'active')",
        )
        .bind(now())
        .bind(q.id)
        .bind(campaign_id)
        .bind(&q.token)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 1 {
            batch.push(q);
        }
    }
    tx.commit().await?;
    Ok(batch)
}

async fn pause(st: &AppState, campaign_id: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE campaigns SET status = 'paused' WHERE id = $1 AND status = 'sending'")
        .bind(campaign_id)
        .execute(&st.db)
        .await?;
    Ok(())
}

/// Sends as many messages for one campaign as the site's rate bucket allows right now.
async fn process_campaign(
    st: &AppState,
    site: &Site,
    campaign_id: i64,
    buckets: &mut RateBuckets,
) -> anyhow::Result<()> {
    // Re-read the campaign now: it may have been paused since the tick started.
    let c: Campaign = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CAMPAIGN_COLS} FROM campaigns WHERE id = $1"
    )))
    .bind(campaign_id)
    .fetch_one(&st.db)
    .await?;
    if c.status != "sending" {
        return Ok(());
    }
    let list: List = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LIST_COLS} FROM lists WHERE id = $1"
    )))
    .bind(c.list_id)
    .fetch_one(&st.db)
    .await?;
    if site.physical_address.trim().is_empty() {
        anyhow::bail!("site has no physical mailing address (required in every footer)");
    }
    let provider = mailer::provider_for(site, &st.http)?;
    let now_ts = now();

    // Recipients who unsubscribed or were suppressed since the campaign started are skipped.
    sqlx::query(
        "UPDATE campaign_sends SET status = 'skipped', sent_at = $1 WHERE campaign_id = $2 AND status = 'queued' \
         AND subscription_id NOT IN (SELECT s.id FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id \
           WHERE s.status = 'confirmed' AND sb.status = 'active')",
    )
    .bind(&now_ts)
    .bind(c.id)
    .execute(&st.db)
    .await?;

    let candidates: Vec<QueuedSend> = sqlx::query_as(
        "SELECT cs.id, cs.attempts, s.token, sb.email, sb.name \
         FROM campaign_sends cs JOIN subscriptions s ON s.id = cs.subscription_id \
         JOIN subscribers sb ON sb.id = s.subscriber_id \
         WHERE cs.campaign_id = $1 AND cs.status = 'queued' AND (cs.next_attempt_at IS NULL OR cs.next_attempt_at <= $2) \
         ORDER BY cs.id LIMIT $3",
    )
    .bind(c.id)
    .bind(&now_ts)
    .bind(site.send_rate.clamp(1, 500))
    .fetch_all(&st.db)
    .await?;

    if candidates.is_empty() {
        let (remaining,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM campaign_sends WHERE campaign_id = $1 AND status IN ('queued', 'sending')",
        )
        .bind(c.id)
        .fetch_one(&st.db)
        .await?;
        if remaining == 0 {
            sqlx::query("UPDATE campaigns SET status = 'sent', finished_at = $1 WHERE id = $2 AND status = 'sending'")
                .bind(now())
                .bind(c.id)
                .execute(&st.db)
                .await?;
            tracing::info!("campaign {} finished", c.id);
        }
        return Ok(());
    }

    // Work in small chunks: each chunk is claimed immediately before its provider calls (so a pause,
    // unsubscribe or token rotation stops everything not yet claimed) and only MAX_IN_FLIGHT rendered
    // messages exist at once. A claim requires the campaign to still be sending and the recipient to
    // still be eligible with the same token; next_attempt_at records the claim time for recovery.
    let mut outcomes: Vec<Option<FailureKind>> = Vec::with_capacity(candidates.len());
    for candidate_chunk in candidates.chunks(MAX_IN_FLIGHT) {
        let granted = buckets.take(site.id, site.send_rate, candidate_chunk.len());
        if granted == 0 {
            break;
        }
        // The chunk's claims commit atomically: on any error the transaction rolls back and every
        // row stays queued, so nothing is left claimed without a provider call.
        let claimed = claim_chunk(st, c.id, &candidate_chunk[..granted]).await;
        let batch: Vec<&QueuedSend> = match claimed {
            Ok(b) => b,
            Err(e) => {
                buckets.refund(site.id, granted);
                return Err(e);
            }
        };
        buckets.refund(site.id, granted - batch.len());
        if batch.is_empty() {
            continue;
        }

        outcomes.extend(join_all(batch.iter().map(|&q| {
        let (provider, list, c) = (&provider, &list, &c);
        async move {
            let email = render_campaign(
                &st.cfg.base_url,
                site,
                list,
                c,
                &Recipient { email: &q.email, name: &q.name, token: &q.token },
            );
            let res = match mailer::build_message(site, &email) {
                Ok(msg) => provider.send(&msg).await,
                Err(e) => Err(SendError::new(FailureKind::Permanent, format!("{e:#}"))),
            };
            let (status, error, message_id, next_at) = match &res {
                Ok(mid) => ("sent", String::new(), mid.clone(), None),
                Err(e) if e.kind == FailureKind::Retryable && q.attempts + 1 < MAX_ATTEMPTS => (
                    "queued",
                    e.message.clone(),
                    String::new(),
                    Some(ts_after(chrono::Duration::seconds(RETRY_BACKOFF_SECS * (q.attempts + 1)))),
                ),
                Err(e) if e.kind == FailureKind::Unknown => ("failed", format!("unknown: {}", e.message), String::new(), None),
                Err(e) => ("failed", e.message.clone(), String::new(), None),
            };
            if let Err(e) = &res {
                tracing::warn!("send {} attempt {} failed ({:?}): {}", q.id, q.attempts + 1, e.kind, e.message);
            }
            // Persist immediately (with a few retries) so a delivered message is never left
            // claimed. If this still fails, abandoned-claim recovery marks it delivery-unknown.
            for attempt in 0..3 {
                let saved = sqlx::query(
                    "UPDATE campaign_sends SET status = $1, error = $2, message_id = $3, sent_at = $4, next_attempt_at = $5 \
                     WHERE id = $6",
                )
                .bind(status)
                .bind(&error)
                .bind(&message_id)
                .bind(now())
                .bind(&next_at)
                .bind(q.id)
                .execute(&st.db)
                .await;
                match saved {
                    Ok(_) => break,
                    Err(e) => {
                        tracing::error!("failed to record result of send {} (try {}): {e}", q.id, attempt + 1);
                        tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
                    }
                }
            }
            res.err().map(|e| e.kind)
        }
    }))
    .await);
    }

    // Every message definitively failing for non-retryable reasons means the provider is likely
    // misconfigured: pause instead of burning through the list. Retryable failures back off instead.
    if outcomes.is_empty() {
        return Ok(());
    }
    let all_failed_hard = outcomes
        .iter()
        .all(|o| matches!(o, Some(FailureKind::Permanent) | Some(FailureKind::Unknown)));
    if all_failed_hard {
        tracing::error!(
            "campaign {}: every message in the batch failed; pausing",
            c.id
        );
        pause(st, c.id).await?;
    }
    Ok(())
}
