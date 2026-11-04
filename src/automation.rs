//! Automations: a trigger plus a tree of steps that each subscriber walks through, edited in the
//! admin UI as a vertical flowchart (like ActiveCampaign's automation builder).
//!
//! Triggers: joining the newsletter, getting a tag, clicking a link in a campaign, or being added
//! by hand. Steps: send an email, wait (a duration, or until a condition is met), if/else on a
//! condition, add/remove a tag, subscribe to/unsubscribe from a newsletter, a goal (people who meet
//! it jump ahead to it from anywhere earlier in the flow), and exit.
//!
//! Email steps are campaign rows with `automation_id` set; this module only queues a
//! `campaign_sends` row when someone reaches one. The normal sender (worker.rs) delivers it with
//! the usual claiming, retries, rate limits, unsubscribe checks, tracking and compliance footer,
//! and only while the automation is active.
//!
//! Like everything else here, the scheduler is driven by database state, not events: every run it
//! enrolls whoever the trigger matched since the automation was first started, then moves each
//! person on from the step they're at (`automation_runs.step_id`, reached at `entered_at`) as far
//! as they can go now. Each move is its own transaction that first locks the automation row and
//! re-checks its `revision`, which every edit bumps in its own transaction, so a decision made from
//! a flowchart that has since changed is dropped and remade next run. People who can't receive
//! mail right now (unsubscribed, bounced) are held where they are and carry on if they come back.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::AuthUser;
use crate::db::{now, random_token, Db};
use crate::error::{AppError, AppResult};
use crate::models::{Campaign, CAMPAIGN_COLS};
use crate::AppState;

type J = AppResult<Json<Value>>;
type Tx<'a> = sqlx::Transaction<'a, sqlx::Any>;

// ---------------------------------------------------------------------------------------
// The flowchart

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Confirms their subscription to this newsletter (CSV imports only if `include_imports`).
    Subscribed {
        #[serde(default)]
        include_imports: bool,
    },
    /// Gets this tag.
    TagAdded { tag_id: i64 },
    /// Clicks a link (any, or the one given) in this campaign.
    Clicked {
        campaign_id: i64,
        #[serde(default)]
        url: String,
    },
    /// Only people added with POST /automations/{id}/enroll.
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    /// Opened (or clicked a link in) this campaign. Opens are approximate.
    Opened {
        campaign_id: i64,
    },
    /// Clicked a link (any, or the one given) in this campaign.
    Clicked {
        campaign_id: i64,
        #[serde(default)]
        url: String,
    },
    HasTag {
        tag_id: i64,
    },
    /// Is a confirmed subscriber of this newsletter.
    OnList {
        list_id: i64,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Minutes,
    Hours,
    Days,
    Weeks,
}

impl Unit {
    fn minutes(self) -> i64 {
        match self {
            Unit::Minutes => 1,
            Unit::Hours => 60,
            Unit::Days => 60 * 24,
            Unit::Weeks => 60 * 24 * 7,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Add,
    Remove,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ListAction {
    Subscribe,
    Unsubscribe,
}

/// What happens at a goal that hasn't been met when someone reaches it in the normal flow.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Unmet {
    #[default]
    Continue,
    Wait,
    Exit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub id: String,
    #[serde(flatten)]
    pub kind: StepKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepKind {
    Email {
        campaign_id: i64,
    },
    /// Waits `amount` `unit`s. With `until`, moves on as soon as the condition is met, or after
    /// `amount` `unit`s at the latest (amount 0 = no time limit).
    Wait {
        #[serde(default)]
        amount: i64,
        unit: Unit,
        #[serde(default)]
        until: Option<Condition>,
    },
    /// If/else: people who meet the condition take `yes`, the rest `no`; both carry on below.
    Branch {
        condition: Condition,
        #[serde(default)]
        yes: Vec<Step>,
        #[serde(default)]
        no: Vec<Step>,
    },
    Tag {
        action: Action,
        tag_id: i64,
    },
    List {
        action: ListAction,
        list_id: i64,
    },
    /// People anywhere earlier in the flow who meet the condition jump straight past it.
    Goal {
        condition: Condition,
        #[serde(default)]
        name: String,
        #[serde(default)]
        unmet: Unmet,
    },
    Exit,
}

pub const MAX_STEPS: usize = 500;
const MAX_DEPTH: usize = 20;
/// Longest wait: about ten years.
pub const MAX_WAIT_MINUTES: i64 = 3650 * 24 * 60;

/// The flowchart flattened for the scheduler.
#[derive(Debug, Default)]
struct Plan<'a> {
    /// Steps in reading order (depth first, yes before no).
    order: Vec<&'a Step>,
    index: HashMap<&'a str, usize>,
    /// The step that follows each one when it completes (None = the end).
    next: HashMap<&'a str, Option<&'a str>>,
    first: Option<&'a str>,
}

impl<'a> Plan<'a> {
    fn new(steps: &'a [Step]) -> Self {
        let mut p = Plan {
            first: steps.first().map(|s| s.id.as_str()),
            ..Default::default()
        };
        p.walk(steps, None);
        p
    }

    fn walk(&mut self, steps: &'a [Step], after: Option<&'a str>) {
        for (i, s) in steps.iter().enumerate() {
            let next = steps.get(i + 1).map(|n| n.id.as_str()).or(after);
            self.index.insert(&s.id, self.order.len());
            self.order.push(s);
            self.next.insert(&s.id, next);
            if let StepKind::Branch { yes, no, .. } = &s.kind {
                self.walk(yes, next);
                self.walk(no, next);
            }
        }
    }

    fn get(&self, id: &str) -> Option<&'a Step> {
        self.index.get(id).map(|&i| self.order[i])
    }

    fn after(&self, id: &str) -> Option<&'a str> {
        self.next.get(id).copied().flatten()
    }

    /// Goals someone at `id` could still reach (through either arm of an if/else ahead of them, but
    /// never into the other arm of one they're already in), in reading order.
    fn goals_after(&self, id: &str) -> Vec<&'a Step> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut todo: Vec<&str> = self.after(id).into_iter().collect();
        if let Some(StepKind::Branch { yes, no, .. }) = self.get(id).map(|s| &s.kind) {
            todo.extend(yes.first().map(|s| s.id.as_str()));
            todo.extend(no.first().map(|s| s.id.as_str()));
        }
        while let Some(cur) = todo.pop() {
            if !seen.insert(cur) {
                continue;
            }
            let Some(step) = self.get(cur) else { continue };
            todo.extend(self.after(cur));
            if let StepKind::Branch { yes, no, .. } = &step.kind {
                todo.extend(yes.first().map(|s| s.id.as_str()));
                todo.extend(no.first().map(|s| s.id.as_str()));
            }
        }
        let mut goals: Vec<&'a Step> = seen
            .into_iter()
            .filter_map(|i| self.get(i))
            .filter(|s| matches!(s.kind, StepKind::Goal { .. }))
            .collect();
        goals.sort_by_key(|s| self.index[s.id.as_str()]);
        goals
    }
}

/// Where someone at `old_id` in the old flowchart continues in the new one: the same step if it
/// still exists, else the first surviving step that would have come after it (None = done).
fn remap(old: &Plan, new_ids: &HashSet<&str>, old_id: &str) -> Option<String> {
    let mut cur = Some(old_id);
    let mut guard = 0;
    while let Some(id) = cur {
        if new_ids.contains(id) {
            return Some(id.to_string());
        }
        guard += 1;
        if guard > MAX_STEPS + 1 {
            return None;
        }
        cur = old.after(id);
    }
    None
}

fn plus_minutes(ts: &str, minutes: i64) -> Option<String> {
    let t = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    Some(
        (t.with_timezone(&chrono::Utc) + chrono::Duration::minutes(minutes))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

// ---------------------------------------------------------------------------------------
// Scheduler

#[derive(Debug, PartialEq)]
enum Effect {
    None,
    Queue(i64),
    /// Re-queue their send of this email, skipped while they couldn't receive mail.
    Restore(i64),
    /// A goal pulled them ahead: drop their not-yet-sent email (if any) of this automation.
    CancelQueued,
    Tag(Action, i64),
    List(ListAction, i64),
}

#[derive(Debug, PartialEq)]
struct Move {
    /// The step they go to (None = done); may be the same step (an email they're now sent).
    to: Option<String>,
    effect: Effect,
    /// When they're considered to have reached `to`, which waits count from. A wait passes on its
    /// due time and other steps on when they were reached, so a schedule keeps its original times
    /// even when people catch up late (after a pause, a send retry or an unsubscribe).
    at: String,
}

struct Ctx<'a> {
    list_id: i64,
    /// Email campaigns of this automation → ready to send (live and written).
    emails: &'a HashMap<i64, bool>,
    now_ts: &'a str,
}

async fn met(db: &Db, c: &Condition, sub_id: i64) -> anyhow::Result<bool> {
    let q = match c {
        Condition::Opened { campaign_id } => sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM engagements WHERE subscription_id = $1 AND campaign_id = $2 AND kind IN ('open', 'click')",
        )
        .bind(sub_id)
        .bind(*campaign_id),
        Condition::Clicked { campaign_id, url } => sqlx::query_as(
            "SELECT COUNT(*) FROM engagements e WHERE e.subscription_id = $1 AND e.campaign_id = $2 AND e.kind = 'click' \
             AND ($3 = '' OR e.link_id IN (SELECT id FROM links WHERE campaign_id = $2 AND url = $3))",
        )
        .bind(sub_id)
        .bind(*campaign_id)
        .bind(url.clone()),
        Condition::HasTag { tag_id } => sqlx::query_as(
            "SELECT COUNT(*) FROM subscriber_tags st JOIN subscriptions s ON s.subscriber_id = st.subscriber_id \
             WHERE s.id = $1 AND st.tag_id = $2",
        )
        .bind(sub_id)
        .bind(*tag_id),
        Condition::OnList { list_id } => sqlx::query_as(
            "SELECT COUNT(*) FROM subscriptions s JOIN subscriptions o ON o.subscriber_id = s.subscriber_id \
             WHERE s.id = $1 AND o.list_id = $2 AND o.status = 'confirmed'",
        )
        .bind(sub_id)
        .bind(*list_id),
    };
    Ok(q.fetch_one(db).await?.0 > 0)
}

/// True when a step still has a tag, newsletter or campaign to choose; people wait before it.
fn step_unset(k: &StepKind) -> bool {
    match k {
        StepKind::Branch { condition, .. } | StepKind::Goal { condition, .. } => {
            condition_unset(condition)
        }
        StepKind::Wait { until: Some(c), .. } => condition_unset(c),
        StepKind::Tag { tag_id, .. } => *tag_id == 0,
        StepKind::List { list_id, .. } => *list_id == 0,
        _ => false,
    }
}

/// The move someone at `step_id` (reached at `entered_at`) can make now; None = stay put.
async fn decide(
    db: &Db,
    plan: &Plan<'_>,
    cx: &Ctx<'_>,
    sub_id: i64,
    step_id: &str,
    entered_at: &str,
) -> anyhow::Result<Option<Move>> {
    let now = cx.now_ts.to_string();
    let Some(step) = plan.get(step_id) else {
        return Ok(Some(Move {
            to: None,
            effect: Effect::None,
            at: now,
        }));
    };
    let next = || plan.after(step_id).map(str::to_string);
    let go = |to: Option<String>, effect, at: &str| {
        Ok(Some(Move {
            to,
            effect,
            at: at.to_string(),
        }))
    };
    // A goal further down that's been met pulls them straight past it.
    for g in plan.goals_after(step_id) {
        if let StepKind::Goal { condition, .. } = &g.kind {
            if !condition_unset(condition) && met(db, condition, sub_id).await? {
                return go(
                    plan.after(&g.id).map(str::to_string),
                    Effect::CancelQueued,
                    &now,
                );
            }
        }
    }
    if step_unset(&step.kind) {
        return Ok(None);
    }
    match &step.kind {
        StepKind::Email { campaign_id } => {
            // An unwritten email, or one paused after send failures, holds people here.
            if !cx.emails.get(campaign_id).copied().unwrap_or(false) {
                return Ok(None);
            }
            // They stay here until it's actually sent (or definitively failed), so steps after it
            // (another email, an unsubscribe) can never overtake it.
            let send: Option<(String,)> = sqlx::query_as(
                "SELECT status FROM campaign_sends WHERE campaign_id = $1 AND subscription_id = $2",
            )
            .bind(*campaign_id)
            .bind(sub_id)
            .fetch_optional(db)
            .await?;
            match send.as_ref().map(|s| s.0.as_str()) {
                None => go(
                    Some(step_id.to_string()),
                    Effect::Queue(*campaign_id),
                    entered_at,
                ),
                Some("skipped") => go(
                    Some(step_id.to_string()),
                    Effect::Restore(*campaign_id),
                    entered_at,
                ),
                Some("sent") | Some("failed") => go(next(), Effect::None, entered_at),
                _ => Ok(None),
            }
        }
        StepKind::Wait {
            amount,
            unit,
            until,
        } => {
            if let Some(c) = until {
                if met(db, c, sub_id).await? {
                    return go(next(), Effect::None, &now);
                }
                if *amount <= 0 {
                    return Ok(None);
                }
            }
            let Some(due) = plus_minutes(entered_at, (*amount).max(0) * unit.minutes()) else {
                return Ok(None);
            };
            if due.as_str() <= cx.now_ts {
                go(next(), Effect::None, &due)
            } else {
                Ok(None)
            }
        }
        StepKind::Branch { condition, yes, no } => {
            let path = if met(db, condition, sub_id).await? {
                yes
            } else {
                no
            };
            go(
                path.first().map(|s| s.id.clone()).or_else(next),
                Effect::None,
                entered_at,
            )
        }
        StepKind::Tag { action, tag_id } => go(next(), Effect::Tag(*action, *tag_id), entered_at),
        StepKind::List { action, list_id } => {
            // Unsubscribing them from this newsletter ends the automation for them.
            let to = if *action == ListAction::Unsubscribe && *list_id == cx.list_id {
                None
            } else {
                next()
            };
            go(to, Effect::List(*action, *list_id), entered_at)
        }
        StepKind::Goal {
            condition, unmet, ..
        } => {
            if met(db, condition, sub_id).await? {
                // Someone held here until now starts the rest from now.
                let at = if *unmet == Unmet::Wait {
                    &now
                } else {
                    entered_at
                };
                return go(next(), Effect::None, at);
            }
            match unmet {
                Unmet::Continue => go(next(), Effect::None, entered_at),
                Unmet::Wait => Ok(None),
                Unmet::Exit => go(None, Effect::None, entered_at),
            }
        }
        StepKind::Exit => go(None, Effect::None, entered_at),
    }
}

/// Locks the automation row and checks it hasn't been edited since `rev` was read.
async fn lock(tx: &mut Tx<'_>, id: i64, rev: i64) -> anyhow::Result<bool> {
    let r =
        sqlx::query("UPDATE automations SET revision = revision WHERE id = $1 AND revision = $2")
            .bind(id)
            .bind(rev)
            .execute(&mut **tx)
            .await?;
    Ok(r.rows_affected() == 1)
}

async fn apply(
    tx: &mut Tx<'_>,
    effect: &Effect,
    sub_id: i64,
    automation_id: i64,
    site_list: i64,
    now_ts: &str,
) -> anyhow::Result<()> {
    match *effect {
        Effect::None => {}
        Effect::CancelQueued => {
            sqlx::query(
                "DELETE FROM campaign_sends WHERE subscription_id = $1 AND status = 'queued' \
                 AND campaign_id IN (SELECT id FROM campaigns WHERE automation_id = $2)",
            )
            .bind(sub_id)
            .bind(automation_id)
            .execute(&mut **tx)
            .await?;
        }
        Effect::Queue(cid) => {
            sqlx::query(
                "INSERT INTO campaign_sends (campaign_id, subscription_id, status) VALUES ($1, $2, 'queued') \
                 ON CONFLICT (campaign_id, subscription_id) DO NOTHING",
            )
            .bind(cid)
            .bind(sub_id)
            .execute(&mut **tx)
            .await?;
        }
        Effect::Restore(cid) => {
            sqlx::query(
                "UPDATE campaign_sends SET status = 'queued', attempts = 0, error = '', next_attempt_at = NULL, sent_at = NULL \
                 WHERE campaign_id = $1 AND subscription_id = $2 AND status = 'skipped'",
            )
            .bind(cid)
            .bind(sub_id)
            .execute(&mut **tx)
            .await?;
        }
        Effect::Tag(Action::Add, tag_id) => {
            // Only tags of the automation's own site.
            let added: Option<(i64,)> = sqlx::query_as(
                "INSERT INTO subscriber_tags (subscriber_id, tag_id, added_at) \
                 SELECT s.subscriber_id, t.id, $3 FROM subscriptions s JOIN lists l ON l.id = s.list_id \
                 JOIN tags t ON t.site_id = l.site_id WHERE s.id = $1 AND t.id = $2 \
                 ON CONFLICT (subscriber_id, tag_id) DO NOTHING RETURNING subscriber_id",
            )
            .bind(sub_id)
            .bind(tag_id)
            .bind(now_ts)
            .fetch_optional(&mut **tx)
            .await?;
            crate::api::record_tag_event(&mut **tx, added, tag_id, now_ts).await?;
        }
        Effect::Tag(Action::Remove, tag_id) => {
            sqlx::query(
                "DELETE FROM subscriber_tags WHERE tag_id = $2 AND subscriber_id = (SELECT subscriber_id FROM subscriptions WHERE id = $1)",
            )
            .bind(sub_id)
            .bind(tag_id)
            .execute(&mut **tx)
            .await?;
        }
        Effect::List(ListAction::Subscribe, list_id) => {
            // Same site only. Never re-subscribes someone who unsubscribed from that newsletter; a
            // pending signup is confirmed (the site owner's automation vouches for consent).
            let target: Option<(i64,)> = sqlx::query_as(
                "SELECT s.subscriber_id FROM subscriptions s JOIN lists l ON l.id = s.list_id JOIN lists t ON t.site_id = l.site_id \
                 WHERE s.id = $1 AND t.id = $2 AND l.id = $3",
            )
            .bind(sub_id)
            .bind(list_id)
            .bind(site_list)
            .fetch_optional(&mut **tx)
            .await?;
            let Some((subscriber_id,)) = target else {
                return Ok(());
            };
            sqlx::query(
                "INSERT INTO subscriptions (list_id, subscriber_id, status, token, consent_source, created_at, confirmed_at) \
                 VALUES ($1, $2, 'confirmed', $3, 'automation', $4, $4) ON CONFLICT (list_id, subscriber_id) DO NOTHING",
            )
            .bind(list_id)
            .bind(subscriber_id)
            .bind(random_token(24))
            .bind(now_ts)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "UPDATE subscriptions SET status = 'confirmed', confirmed_at = $3 WHERE list_id = $1 AND subscriber_id = $2 AND status = 'pending'",
            )
            .bind(list_id)
            .bind(subscriber_id)
            .bind(now_ts)
            .execute(&mut **tx)
            .await?;
        }
        Effect::List(ListAction::Unsubscribe, list_id) => {
            sqlx::query(
                "UPDATE subscriptions SET status = 'unsubscribed', unsubscribed_at = $3 WHERE list_id = $1 \
                 AND subscriber_id = (SELECT subscriber_id FROM subscriptions WHERE id = $2) AND status <> 'unsubscribed' \
                 AND list_id IN (SELECT t.id FROM lists t JOIN lists l ON l.site_id = t.site_id WHERE l.id = $4)",
            )
            .bind(list_id)
            .bind(sub_id)
            .bind(now_ts)
            .bind(site_list)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// Enrolls whoever the trigger matched, at the first step.
async fn enroll(
    tx: &mut Tx<'_>,
    a: &AutomationRow,
    trigger: &Trigger,
    first: Option<&str>,
    now_ts: &str,
) -> anyhow::Result<u64> {
    let Some(started) = a.started_at.as_deref() else {
        return Ok(0);
    };
    let (step, status) = match first {
        Some(f) => (f, "active"),
        None => ("", "done"),
    };
    let base = "INSERT INTO automation_runs (automation_id, subscription_id, status, step_id, entered_at, created_at) \
         SELECT $1, s.id, $2, $3, $4, $4 FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id \
         WHERE s.list_id = $5 AND s.status = 'confirmed' AND sb.status = 'active' \
         AND NOT EXISTS (SELECT 1 FROM automation_runs r WHERE r.automation_id = $1 AND r.subscription_id = s.id) AND ";
    let q = |cond: &str| {
        format!("{base}{cond} ON CONFLICT (automation_id, subscription_id) DO NOTHING")
    };
    let bind = |sql: String| {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(a.id)
            .bind(status)
            .bind(step.to_string())
            .bind(now_ts.to_string())
            .bind(a.list_id)
            .bind(started.to_string())
    };
    let r = match trigger {
        Trigger::Manual => return Ok(0),
        Trigger::Subscribed { include_imports } => {
            bind(q("s.confirmed_at IS NOT NULL AND s.confirmed_at >= $6 AND ($7 OR s.consent_source <> 'import')"))
                .bind(*include_imports)
                .execute(&mut **tx)
                .await?
        }
        Trigger::TagAdded { tag_id } => {
            bind(q("EXISTS (SELECT 1 FROM tag_events te WHERE te.subscriber_id = s.subscriber_id AND te.tag_id = $7 AND te.added_at >= $6)"))
                .bind(*tag_id)
                .execute(&mut **tx)
                .await?
        }
        Trigger::Clicked { campaign_id, url } => {
            bind(q("EXISTS (SELECT 1 FROM engagements e WHERE e.subscription_id = s.id AND e.campaign_id = $7 AND e.kind = 'click' \
                    AND e.created_at >= $6 AND ($8 = '' OR e.link_id IN (SELECT id FROM links WHERE campaign_id = $7 AND url = $8)))"))
                .bind(*campaign_id)
                .bind(url.clone())
                .execute(&mut **tx)
                .await?
        }
    };
    Ok(r.rows_affected())
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct AutomationRow {
    id: i64,
    list_id: i64,
    trigger_config: String,
    steps: String,
    started_at: Option<String>,
    revision: i64,
}

/// Enrolls newly triggered subscribers and moves everyone on as far as they can go now.
/// Returns the number of moves made. Safe to run repeatedly and concurrently with edits.
pub async fn advance(db: &Db, now_ts: &str) -> anyhow::Result<usize> {
    let autos: Vec<AutomationRow> = sqlx::query_as(
        "SELECT id, list_id, trigger_config, steps, started_at, revision FROM automations WHERE status = 'active' ORDER BY id",
    )
    .fetch_all(db)
    .await?;
    let mut moves = 0;
    for a in autos {
        match advance_one(db, &a, now_ts).await {
            Ok(n) => moves += n,
            Err(e) => tracing::error!("automation {}: {e:#}", a.id),
        }
    }
    Ok(moves)
}

/// Most moves one person makes in a single run (a flowchart is finite, but be safe).
const MAX_MOVES_PER_RUN: usize = 200;

async fn advance_one(db: &Db, a: &AutomationRow, now_ts: &str) -> anyhow::Result<usize> {
    let steps: Vec<Step> = serde_json::from_str(&a.steps)?;
    let trigger: Trigger = serde_json::from_str(&a.trigger_config)?;
    let plan = Plan::new(&steps);

    let mut tx = db.begin().await?;
    if !lock(&mut tx, a.id, a.revision).await? {
        return Ok(0); // edited since we read it; next run
    }
    enroll(&mut tx, a, &trigger, plan.first, now_ts).await?;
    tx.commit().await?;

    let emails: HashMap<i64, bool> = sqlx::query_as::<_, (i64, String, String, String)>(
        "SELECT id, status, subject, html_body FROM campaigns WHERE automation_id = $1",
    )
    .bind(a.id)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|(id, status, subject, body)| {
        (
            id,
            status == "sending" && !subject.trim().is_empty() && !body.trim().is_empty(),
        )
    })
    .collect();
    let cx = Ctx {
        list_id: a.list_id,
        emails: &emails,
        now_ts,
    };
    // Only people who can receive mail right now; the rest stay where they are.
    let runs: Vec<(i64, i64, String, String)> = sqlx::query_as(
        "SELECT r.id, r.subscription_id, r.step_id, r.entered_at FROM automation_runs r \
         JOIN subscriptions s ON s.id = r.subscription_id JOIN subscribers sb ON sb.id = s.subscriber_id \
         WHERE r.automation_id = $1 AND r.status = 'active' AND s.status = 'confirmed' AND sb.status = 'active' ORDER BY r.id",
    )
    .bind(a.id)
    .fetch_all(db)
    .await?;

    let mut moves = 0;
    'runs: for (run_id, sub_id, mut step_id, mut entered_at) in runs {
        for _ in 0..MAX_MOVES_PER_RUN {
            let Some(m) = decide(db, &plan, &cx, sub_id, &step_id, &entered_at).await? else {
                continue 'runs;
            };
            let mut tx = db.begin().await?;
            if !lock(&mut tx, a.id, a.revision).await? {
                return Ok(moves);
            }
            let to = m.to.clone().unwrap_or_default();
            let done = m.to.is_none();
            let r = sqlx::query(
                "UPDATE automation_runs SET step_id = $1, entered_at = $2, status = $3, finished_at = $4 \
                 WHERE id = $5 AND step_id = $6 AND status = 'active' \
                 AND EXISTS (SELECT 1 FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id \
                   WHERE s.id = automation_runs.subscription_id AND s.status = 'confirmed' AND sb.status = 'active')",
            )
            .bind(&to)
            .bind(&m.at)
            .bind(if done { "done" } else { "active" })
            .bind(if done { Some(now_ts) } else { None })
            .bind(run_id)
            .bind(&step_id)
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() != 1 {
                continue 'runs; // moved by someone else meanwhile
            }
            apply(&mut tx, &m.effect, sub_id, a.id, a.list_id, now_ts).await?;
            tx.commit().await?;
            moves += 1;
            if done {
                continue 'runs;
            }
            step_id = to;
            entered_at = m.at;
        }
    }
    Ok(moves)
}

// ---------------------------------------------------------------------------------------
// Converting email courses (the previous version of this feature)

/// Turns each email course into an automation: joined-the-newsletter trigger, and each lesson an
/// email step preceded by a wait for the gap since the previous one. People part-way through a
/// course carry on from where they are, on the same schedule.
pub async fn convert_courses(db: &Db) -> anyhow::Result<()> {
    let seqs: Vec<(i64, i64, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, list_id, name, status, created_at, started_at FROM sequences \
         WHERE id NOT IN (SELECT legacy_sequence_id FROM automations WHERE legacy_sequence_id IS NOT NULL) ORDER BY id",
    )
    .fetch_all(db)
    .await?;
    for (seq_id, list_id, name, status, created_at, started_at) in seqs {
        let lessons: Vec<(i64, i64, String)> = sqlx::query_as(
            "SELECT id, delay_days, created_at FROM campaigns WHERE sequence_id = $1 ORDER BY delay_days, id",
        )
        .bind(seq_id)
        .fetch_all(db)
        .await?;
        let mut steps = Vec::new();
        /// A lesson's place in the converted flow.
        struct At {
            cid: i64,
            day: i64,
            prev_day: i64,
            created_at: String,
            /// The wait before its email, if any (anchored at the previous lesson's day).
            wait: Option<usize>,
            email: usize,
        }
        let mut at = Vec::new();
        let mut prev = 0;
        for (cid, day, lesson_created) in &lessons {
            let wait = (*day > prev).then(|| {
                steps.push(Step {
                    id: random_token(6),
                    kind: StepKind::Wait {
                        amount: day - prev,
                        unit: Unit::Days,
                        until: None,
                    },
                });
                steps.len() - 1
            });
            steps.push(Step {
                id: random_token(6),
                kind: StepKind::Email { campaign_id: *cid },
            });
            at.push(At {
                cid: *cid,
                day: *day,
                prev_day: prev,
                created_at: lesson_created.clone(),
                wait,
                email: steps.len() - 1,
            });
            prev = (*day).max(prev);
        }
        let mut tx = db.begin().await?;
        let (aid,): (i64,) = sqlx::query_as(
            "INSERT INTO automations (list_id, name, status, trigger_config, steps, created_at, started_at, legacy_sequence_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
        )
        .bind(list_id)
        .bind(&name)
        .bind(&status)
        .bind(serde_json::to_string(&Trigger::Subscribed { include_imports: false })?)
        .bind(serde_json::to_string(&steps)?)
        .bind(&created_at)
        .bind(&started_at)
        .bind(seq_id)
        .fetch_one(&mut *tx)
        .await?;
        let enrollments: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT subscription_id, enrolled_at, status FROM sequence_enrollments WHERE sequence_id = $1",
        )
        .bind(seq_id)
        .fetch_all(&mut *tx)
        .await?;
        for (sub_id, enrolled_at, st) in enrollments {
            let sends: HashMap<i64, String> = sqlx::query_as::<_, (i64, String)>(
                "SELECT cs.campaign_id, cs.status FROM campaign_sends cs JOIN campaigns c ON c.id = cs.campaign_id \
                 WHERE c.sequence_id = $1 AND cs.subscription_id = $2",
            )
            .bind(seq_id)
            .bind(sub_id)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();
            let day_at =
                |d: i64| plus_minutes(&enrolled_at, d * 24 * 60).unwrap_or(enrolled_at.clone());
            // Where the course would pick up for them (as its scheduler did): the first lesson in
            // order that wasn't delivered or definitively failed, skipping lessons added after their
            // day had passed. One already on its way (or skipped while they were away) is waited
            // for at its email step; one not started yet at the wait before it, on its old day.
            let mut place = at
                .iter()
                .find(|l| {
                    matches!(
                        sends.get(&l.cid).map(String::as_str),
                        Some("queued") | Some("sending")
                    )
                })
                .map(|l| (l.email, day_at(l.day)));
            let rest: &[At] = if place.is_some() { &[] } else { &at };
            for l in rest {
                match sends.get(&l.cid).map(String::as_str) {
                    Some("sent") | Some("failed") => continue,
                    None if day_at(l.day) < l.created_at => continue,
                    None => {
                        place = Some(match l.wait {
                            Some(w) => (w, day_at(l.prev_day)),
                            None => (l.email, day_at(l.day)),
                        })
                    }
                    Some(_) => place = Some((l.email, day_at(l.day))),
                }
                break;
            }
            let (idx, entered) = place.unwrap_or((usize::MAX, enrolled_at.clone()));
            let (status, step) = match steps.get(idx) {
                Some(s) if st == "active" => ("active", s.id.clone()),
                _ => ("done", String::new()),
            };
            sqlx::query(
                "INSERT INTO automation_runs (automation_id, subscription_id, status, step_id, entered_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (automation_id, subscription_id) DO NOTHING",
            )
            .bind(aid)
            .bind(sub_id)
            .bind(status)
            .bind(step)
            .bind(entered)
            .bind(&enrolled_at)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE campaigns SET automation_id = $1, sequence_id = NULL WHERE sequence_id = $2",
        )
        .bind(aid)
        .bind(seq_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM sequences WHERE id = $1")
            .bind(seq_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!("converted email course {seq_id} into automation {aid}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// API

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Automation {
    pub id: i64,
    pub list_id: i64,
    pub name: String,
    pub status: String,
    #[serde(skip)]
    pub trigger_config: String,
    #[serde(skip)]
    pub steps: String,
    pub created_at: String,
    pub started_at: Option<String>,
}

const AUTOMATION_COLS: &str =
    "id, list_id, name, status, trigger_config, steps, created_at, started_at";

async fn automation_for(st: &AppState, u: &AuthUser, id: i64) -> AppResult<Automation> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT a.{} FROM automations a JOIN lists l ON l.id = a.list_id JOIN sites s ON s.id = l.site_id \
         WHERE a.id = $1 AND s.account_id = $2",
        AUTOMATION_COLS.replace(", ", ", a.")
    )))
    .bind(id)
    .bind(u.account_id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(AppError::not_found)
}

async fn automation_json(st: &AppState, a: &Automation, full: bool) -> AppResult<Value> {
    let (total, active, done): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(CASE WHEN status = 'active' THEN 1 END), COUNT(CASE WHEN status = 'done' THEN 1 END) \
         FROM automation_runs WHERE automation_id = $1",
    )
    .bind(a.id)
    .fetch_one(&st.db)
    .await?;
    let mut v = serde_json::to_value(a).unwrap();
    v["trigger"] = serde_json::from_str(&a.trigger_config).unwrap_or(Value::Null);
    v["steps"] = serde_json::from_str(&a.steps).unwrap_or(json!([]));
    v["stats"] = json!({ "enrolled": total, "in_progress": active, "completed": done });
    if full {
        let emails: Vec<Campaign> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {CAMPAIGN_COLS} FROM campaigns WHERE automation_id = $1 ORDER BY id"
        )))
        .bind(a.id)
        .fetch_all(&st.db)
        .await?;
        let mut em = serde_json::Map::new();
        for c in &emails {
            em.insert(c.id.to_string(), crate::api::campaign_json(st, c).await?);
        }
        v["emails"] = Value::Object(em);
        let at: Vec<(String, i64)> = sqlx::query_as(
            "SELECT step_id, COUNT(*) FROM automation_runs WHERE automation_id = $1 AND status = 'active' GROUP BY step_id",
        )
        .bind(a.id)
        .fetch_all(&st.db)
        .await?;
        v["at_step"] = Value::Object(at.into_iter().map(|(k, n)| (k, json!(n))).collect());
    }
    Ok(v)
}

pub async fn list(State(st): State<AppState>, u: AuthUser, Path(list_id): Path<i64>) -> J {
    crate::api::list_for(&st, &u, list_id).await?;
    let rows: Vec<Automation> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {AUTOMATION_COLS} FROM automations WHERE list_id = $1 ORDER BY id"
    )))
    .bind(list_id)
    .fetch_all(&st.db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for a in &rows {
        out.push(automation_json(&st, a, false).await?);
    }
    Ok(Json(Value::Array(out)))
}

#[derive(Deserialize)]
pub struct CreateInput {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub trigger: Option<Trigger>,
}

pub async fn create(
    State(st): State<AppState>,
    u: AuthUser,
    Path(list_id): Path<i64>,
    Json(i): Json<CreateInput>,
) -> J {
    let (_, site) = crate::api::list_for(&st, &u, list_id).await?;
    let mut trigger = i.trigger.unwrap_or(Trigger::Subscribed {
        include_imports: false,
    });
    normalize_trigger(&mut trigger)?;
    let mut tx = st.db.begin().await?;
    lock_list(&mut tx, list_id).await?;
    validate_trigger(&mut tx, &site, list_id, &trigger).await?;
    let name = match i.name.trim() {
        "" => "New automation",
        n => n,
    };
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO automations (list_id, name, status, trigger_config, steps, created_at) VALUES ($1, $2, 'draft', $3, '[]', $4) RETURNING id",
    )
    .bind(list_id)
    .bind(name.chars().take(200).collect::<String>())
    .bind(serde_json::to_string(&trigger).unwrap())
    .bind(now())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    get(State(st), u, Path(id)).await
}

pub async fn get(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    let a = automation_for(&st, &u, id).await?;
    Ok(Json(automation_json(&st, &a, true).await?))
}

async fn owned(
    db: &mut sqlx::AnyConnection,
    sql: &'static str,
    id: i64,
    scope: i64,
) -> AppResult<bool> {
    let (n,): (i64,) = sqlx::query_as(sql)
        .bind(id)
        .bind(scope)
        .fetch_one(&mut *db)
        .await?;
    Ok(n > 0)
}

// An id of 0 means "not chosen yet": allowed while editing, but the automation can't be started
// until it's set (see `unset`).

async fn check_campaign(db: &mut sqlx::AnyConnection, list_id: i64, cid: i64) -> AppResult<()> {
    if cid != 0
        && !owned(
            &mut *db,
            "SELECT COUNT(*) FROM campaigns WHERE id = $1 AND list_id = $2",
            cid,
            list_id,
        )
        .await?
    {
        return Err(AppError::bad_request(format!(
            "campaign {cid} is not in this newsletter"
        )));
    }
    Ok(())
}

async fn check_tag(db: &mut sqlx::AnyConnection, site_id: i64, tag_id: i64) -> AppResult<()> {
    if tag_id != 0
        && !owned(
            &mut *db,
            "SELECT COUNT(*) FROM tags WHERE id = $1 AND site_id = $2",
            tag_id,
            site_id,
        )
        .await?
    {
        return Err(AppError::bad_request(format!(
            "tag {tag_id} doesn't exist on this site"
        )));
    }
    Ok(())
}

async fn validate_condition(
    db: &mut sqlx::AnyConnection,
    site: &crate::models::Site,
    list_id: i64,
    c: &Condition,
) -> AppResult<()> {
    match c {
        Condition::Opened { campaign_id } | Condition::Clicked { campaign_id, .. } => {
            check_campaign(&mut *db, list_id, *campaign_id).await
        }
        Condition::HasTag { tag_id } => check_tag(&mut *db, site.id, *tag_id).await,
        Condition::OnList { list_id: l } => {
            if *l != 0
                && !owned(
                    &mut *db,
                    "SELECT COUNT(*) FROM lists WHERE id = $1 AND site_id = $2",
                    *l,
                    site.id,
                )
                .await?
            {
                return Err(AppError::bad_request(format!(
                    "newsletter {l} isn't on this site"
                )));
            }
            Ok(())
        }
    }
}

async fn validate_trigger(
    db: &mut sqlx::AnyConnection,
    site: &crate::models::Site,
    list_id: i64,
    t: &Trigger,
) -> AppResult<()> {
    match t {
        Trigger::Subscribed { .. } | Trigger::Manual => Ok(()),
        Trigger::TagAdded { tag_id } => check_tag(&mut *db, site.id, *tag_id).await,
        Trigger::Clicked { campaign_id, .. } => {
            check_campaign(&mut *db, list_id, *campaign_id).await
        }
    }
}

/// Serializes saves and deletions that check campaign references (`check_unused`) within one
/// newsletter, so a check can't pass while another request is adding a reference.
pub async fn lock_list(db: &mut sqlx::AnyConnection, list_id: i64) -> AppResult<()> {
    sqlx::query("UPDATE lists SET name = name WHERE id = $1")
        .bind(list_id)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Fails when another automation of the newsletter has a trigger or condition on campaign `cid`
/// (deleting it would also delete the opens and clicks those rely on).
pub async fn check_unused(
    db: &mut sqlx::AnyConnection,
    list_id: i64,
    cid: i64,
    except_automation: i64,
) -> AppResult<()> {
    fn in_steps(steps: &[Step], cid: i64) -> bool {
        let uses = |c: &Condition| matches!(c, Condition::Opened { campaign_id } | Condition::Clicked { campaign_id, .. } if *campaign_id == cid);
        steps.iter().any(|s| match &s.kind {
            StepKind::Branch { condition, yes, no } => {
                uses(condition) || in_steps(yes, cid) || in_steps(no, cid)
            }
            StepKind::Goal { condition, .. } => uses(condition),
            StepKind::Wait { until: Some(c), .. } => uses(c),
            _ => false,
        })
    }
    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT id, name, trigger_config, steps FROM automations WHERE list_id = $1 AND id <> $2",
    )
    .bind(list_id)
    .bind(except_automation)
    .fetch_all(&mut *db)
    .await?;
    for (_, name, trigger, steps) in rows {
        let t: Option<Trigger> = serde_json::from_str(&trigger).ok();
        let steps: Vec<Step> = serde_json::from_str(&steps).unwrap_or_default();
        if matches!(t, Some(Trigger::Clicked { campaign_id, .. }) if campaign_id == cid)
            || in_steps(&steps, cid)
        {
            return Err(AppError::bad_request(format!(
                "the automation “{name}” uses this email's opens or clicks; change it first"
            )));
        }
    }
    Ok(())
}

/// Link rules are compared with tracked links as the URL parser writes them (`https://x.com` is
/// stored as `https://x.com/`), so they're saved the same way. Blank = any link.
fn normalize_url(u: &mut String) -> AppResult<()> {
    let t = u.trim();
    *u = if t.is_empty() {
        String::new()
    } else {
        url::Url::parse(t)
            .ok()
            .filter(|p| matches!(p.scheme(), "http" | "https"))
            .map(String::from)
            .ok_or_else(|| AppError::bad_request(format!("“{t}” isn't a valid http(s) link")))?
    };
    Ok(())
}

fn normalize_condition(c: &mut Condition) -> AppResult<()> {
    if let Condition::Clicked { url, .. } = c {
        normalize_url(url)?;
    }
    Ok(())
}

fn normalize_trigger(t: &mut Trigger) -> AppResult<()> {
    if let Trigger::Clicked { url, .. } = t {
        normalize_url(url)?;
    }
    Ok(())
}

fn normalize_steps(steps: &mut [Step]) -> AppResult<()> {
    for s in steps {
        match &mut s.kind {
            StepKind::Branch { condition, yes, no } => {
                normalize_condition(condition)?;
                normalize_steps(yes)?;
                normalize_steps(no)?;
            }
            StepKind::Goal { condition, .. } => normalize_condition(condition)?,
            StepKind::Wait { until: Some(c), .. } => normalize_condition(c)?,
            _ => {}
        }
    }
    Ok(())
}

fn condition_unset(c: &Condition) -> bool {
    match c {
        Condition::Opened { campaign_id } | Condition::Clicked { campaign_id, .. } => {
            *campaign_id == 0
        }
        Condition::HasTag { tag_id } => *tag_id == 0,
        Condition::OnList { list_id } => *list_id == 0,
    }
}

/// How many things (trigger and steps) still need a tag, newsletter or campaign chosen.
fn unset(trigger: &Trigger, steps: &[Step]) -> usize {
    fn count(steps: &[Step]) -> usize {
        steps
            .iter()
            .map(|s| match &s.kind {
                StepKind::Wait { until: Some(c), .. } => condition_unset(c) as usize,
                StepKind::Branch { condition, yes, no } => {
                    condition_unset(condition) as usize + count(yes) + count(no)
                }
                StepKind::Goal { condition, .. } => condition_unset(condition) as usize,
                StepKind::Tag { tag_id, .. } => (*tag_id == 0) as usize,
                StepKind::List { list_id, .. } => (*list_id == 0) as usize,
                _ => 0,
            })
            .sum()
    }
    let t = match trigger {
        Trigger::TagAdded { tag_id } => *tag_id == 0,
        Trigger::Clicked { campaign_id, .. } => *campaign_id == 0,
        _ => false,
    };
    t as usize + count(steps)
}

/// Checks structure and that everything referenced belongs to this site; returns the email
/// campaigns used.
async fn validate_steps(
    db: &mut sqlx::AnyConnection,
    site: &crate::models::Site,
    a: &Automation,
    steps: &[Step],
) -> AppResult<HashSet<i64>> {
    fn collect<'a>(steps: &'a [Step], depth: usize, out: &mut Vec<&'a Step>) -> AppResult<()> {
        if depth > MAX_DEPTH {
            return Err(AppError::bad_request("if/else steps are nested too deeply"));
        }
        for s in steps {
            out.push(s);
            if let StepKind::Branch { yes, no, .. } = &s.kind {
                collect(yes, depth + 1, out)?;
                collect(no, depth + 1, out)?;
            }
        }
        Ok(())
    }
    let mut all = Vec::new();
    collect(steps, 0, &mut all)?;
    if all.len() > MAX_STEPS {
        return Err(AppError::bad_request(format!("at most {MAX_STEPS} steps")));
    }
    let mut ids = HashSet::new();
    let mut emails = HashSet::new();
    let mut referenced = Vec::new();
    for s in &all {
        let conds = match &s.kind {
            StepKind::Branch { condition, .. } | StepKind::Goal { condition, .. } => {
                vec![condition]
            }
            StepKind::Wait { until: Some(c), .. } => vec![c],
            _ => vec![],
        };
        for c in conds {
            if let Condition::Opened { campaign_id } | Condition::Clicked { campaign_id, .. } = c {
                referenced.push(*campaign_id);
            }
        }
    }
    for s in all {
        if s.id.is_empty() || s.id.len() > 64 || !ids.insert(s.id.as_str()) {
            return Err(AppError::bad_request(
                "every step needs a unique id (1–64 characters)",
            ));
        }
        match &s.kind {
            StepKind::Email { campaign_id } => {
                if !owned(
                    &mut *db,
                    "SELECT COUNT(*) FROM campaigns WHERE id = $1 AND automation_id = $2",
                    *campaign_id,
                    a.id,
                )
                .await?
                {
                    return Err(AppError::bad_request(format!(
                        "email {campaign_id} doesn't belong to this automation (create it with POST /api/automations/{}/emails)",
                        a.id
                    )));
                }
                if !emails.insert(*campaign_id) {
                    return Err(AppError::bad_request(
                        "each email can only be used in one step",
                    ));
                }
            }
            StepKind::Wait {
                amount,
                unit,
                until,
            } => {
                if *amount < 0 || amount.saturating_mul(unit.minutes()) > MAX_WAIT_MINUTES {
                    return Err(AppError::bad_request("waits are between 0 and 10 years"));
                }
                if let Some(c) = until {
                    validate_condition(&mut *db, site, a.list_id, c).await?;
                }
            }
            StepKind::Branch { condition, .. } | StepKind::Goal { condition, .. } => {
                validate_condition(&mut *db, site, a.list_id, condition).await?
            }
            StepKind::Tag { tag_id, .. } => check_tag(&mut *db, site.id, *tag_id).await?,
            StepKind::List { list_id, .. } => {
                validate_condition(
                    &mut *db,
                    site,
                    a.list_id,
                    &Condition::OnList { list_id: *list_id },
                )
                .await?
            }
            StepKind::Exit => {}
        }
    }
    // An open/click condition on one of this automation's emails needs that email to stay (removing
    // it deletes the email and its opens and clicks).
    for cid in referenced {
        if cid != 0
            && !emails.contains(&cid)
            && owned(
                &mut *db,
                "SELECT COUNT(*) FROM campaigns WHERE id = $1 AND automation_id = $2",
                cid,
                a.id,
            )
            .await?
        {
            return Err(AppError::bad_request(
                "a condition still uses this email's opens or clicks; change that condition first",
            ));
        }
    }
    Ok(emails)
}

#[derive(Deserialize)]
pub struct UpdateInput {
    pub name: Option<String>,
    pub trigger: Option<Trigger>,
    pub steps: Option<Vec<Step>>,
    /// `active` starts (or resumes) it; `paused` stops it.
    pub status: Option<String>,
}

/// Marks the automation as changed; see `advance`. Call inside the transaction making the change.
pub async fn bump_revision(tx: &mut Tx<'_>, id: i64) -> AppResult<()> {
    sqlx::query("UPDATE automations SET revision = revision + 1 WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn update(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(i): Json<UpdateInput>,
) -> J {
    let mut i = i;
    let a = automation_for(&st, &u, id).await?;
    let (_, site) = crate::api::list_for(&st, &u, a.list_id).await?;
    if let Some(t) = &mut i.trigger {
        normalize_trigger(t)?;
    }
    if let Some(steps) = &mut i.steps {
        normalize_steps(steps)?;
    }
    if i.status.as_deref() == Some("active") {
        if site.physical_address.trim().is_empty() {
            return Err(AppError::bad_request(
                "set a physical mailing address on the site first (required by CAN-SPAM and most providers)",
            ));
        }
        crate::mailer::provider_for(&site, &st.http)
            .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
    }

    let mut tx = st.db.begin().await?;
    // Newsletter first, then the automation: the same order as deletions, so they can't deadlock.
    lock_list(&mut tx, a.list_id).await?;
    bump_revision(&mut tx, id).await?;
    // Validated once this save holds the automation's lock, so a concurrent save (say, one that
    // removed an email) has either fully happened or not started.
    if let Some(t) = &i.trigger {
        validate_trigger(&mut tx, &site, a.list_id, t).await?;
    }
    let used = match &i.steps {
        Some(steps) => Some(validate_steps(&mut tx, &site, &a, steps).await?),
        None => None,
    };
    // Re-read inside the transaction: remapping must start from the flowchart people are on now.
    let (old_steps, old_trigger): (String, String) =
        sqlx::query_as("SELECT steps, trigger_config FROM automations WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if let Some(name) = i.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        sqlx::query("UPDATE automations SET name = $1 WHERE id = $2")
            .bind(name.chars().take(200).collect::<String>())
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(t) = &i.trigger {
        let new = serde_json::to_string(t).unwrap();
        if new != old_trigger {
            sqlx::query("UPDATE automations SET trigger_config = $1 WHERE id = $2")
                .bind(new)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
    }
    let current: Vec<Step> = match (&i.steps, used) {
        (Some(steps), Some(used)) => {
            let old: Vec<Step> = serde_json::from_str(&old_steps).unwrap_or_default();
            let old_plan = Plan::new(&old);
            let new_plan = Plan::new(steps);
            let new_ids: HashSet<&str> = new_plan.index.keys().copied().collect();
            // People at a step that was removed carry on from what came after it.
            let at: Vec<(i64, String)> = sqlx::query_as(
                "SELECT id, step_id FROM automation_runs WHERE automation_id = $1 AND status = 'active'",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            let ts = now();
            for (run_id, step_id) in at {
                if new_ids.contains(step_id.as_str()) {
                    continue;
                }
                let to = remap(&old_plan, &new_ids, &step_id);
                sqlx::query(
                    "UPDATE automation_runs SET step_id = $1, entered_at = $2, status = $3, finished_at = $4 WHERE id = $5",
                )
                .bind(to.clone().unwrap_or_default())
                .bind(&ts)
                .bind(if to.is_some() { "active" } else { "done" })
                .bind(if to.is_some() { None } else { Some(ts.clone()) })
                .bind(run_id)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query("UPDATE automations SET steps = $1 WHERE id = $2")
                .bind(serde_json::to_string(steps).unwrap())
                .bind(id)
                .execute(&mut *tx)
                .await?;
            // Emails no longer in the flowchart go, with their unsent queue.
            let owned_emails: Vec<(i64,)> =
                sqlx::query_as("SELECT id FROM campaigns WHERE automation_id = $1")
                    .bind(id)
                    .fetch_all(&mut *tx)
                    .await?;
            let trigger: Option<Trigger> = match &i.trigger {
                Some(t) => Some(t.clone()),
                None => serde_json::from_str(&old_trigger).ok(),
            };
            for (cid,) in owned_emails {
                if !used.contains(&cid) {
                    if matches!(trigger, Some(Trigger::Clicked { campaign_id, .. }) if campaign_id == cid)
                    {
                        return Err(AppError::bad_request(
                            "the trigger uses this email's clicks; change the trigger first",
                        ));
                    }
                    check_unused(&mut tx, a.list_id, cid, id).await?;
                    sqlx::query("DELETE FROM campaigns WHERE id = $1")
                        .bind(cid)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            steps.clone()
        }
        _ => serde_json::from_str(&old_steps).unwrap_or_default(),
    };
    match i.status.as_deref() {
        None => {}
        Some("active") => {
            if current.is_empty() {
                return Err(AppError::bad_request("add at least one step first"));
            }
            let trigger = match &i.trigger {
                Some(t) => t.clone(),
                None => serde_json::from_str(&old_trigger).unwrap_or(Trigger::Manual),
            };
            let missing = unset(&trigger, &current);
            if missing > 0 {
                return Err(AppError::bad_request(format!(
                    "choose the tag, newsletter or campaign for {missing} step{} (or the trigger) first",
                    if missing == 1 { "" } else { "s" }
                )));
            }
            let (unwritten,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM campaigns WHERE automation_id = $1 AND (TRIM(subject) = '' OR TRIM(html_body) = '')",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if unwritten > 0 {
                return Err(AppError::bad_request(format!(
                    "{unwritten} email{} still need{} a subject and body",
                    if unwritten == 1 { "" } else { "s" },
                    if unwritten == 1 { "s" } else { "" }
                )));
            }
            sqlx::query(
                "UPDATE automations SET status = 'active', started_at = COALESCE(started_at, $1) WHERE id = $2",
            )
            .bind(now())
            .bind(id)
            .execute(&mut *tx)
            .await?;
            // Also resumes emails that paused themselves after send failures.
            sqlx::query("UPDATE campaigns SET status = 'sending' WHERE automation_id = $1 AND status = 'paused'")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        Some("paused") => {
            sqlx::query(
                "UPDATE automations SET status = 'paused' WHERE id = $1 AND status = 'active'",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        Some(_) => return Err(AppError::bad_request("status must be active or paused")),
    }
    tx.commit().await?;
    get(State(st), u, Path(id)).await
}

pub async fn delete(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    let a = automation_for(&st, &u, id).await?;
    let mut tx = st.db.begin().await?;
    lock_list(&mut tx, a.list_id).await?;
    let emails: Vec<(i64,)> = sqlx::query_as("SELECT id FROM campaigns WHERE automation_id = $1")
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
    for (cid,) in emails {
        check_unused(&mut tx, a.list_id, cid, id).await?;
    }
    // Its emails, their unsent queue and the runs go with it (ON DELETE CASCADE).
    sqlx::query("DELETE FROM campaigns WHERE automation_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM automations WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
pub struct EmailInput {
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub html_body: String,
}

/// Creates an email for this automation, to be placed in a step with PUT /automations/{id}.
pub async fn create_email(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(i): Json<EmailInput>,
) -> J {
    let a = automation_for(&st, &u, id).await?;
    let blank = |v: &str| {
        if v.trim().is_empty() {
            String::new()
        } else {
            v.to_string()
        }
    };
    // Always 'sending': the worker delivers whatever the scheduler queues for it.
    let (cid,): (i64,) = sqlx::query_as(
        "INSERT INTO campaigns (list_id, subject, html_body, status, created_at, started_at, automation_id) \
         VALUES ($1, $2, $3, 'sending', $4, $4, $5) RETURNING id",
    )
    .bind(a.list_id)
    .bind(i.subject.trim())
    .bind(blank(&i.html_body))
    .bind(now())
    .bind(id)
    .fetch_one(&st.db)
    .await?;
    crate::api::get_campaign(State(st), u, Path(cid)).await
}

#[derive(Deserialize)]
pub struct EnrollInput {
    /// `all` = every confirmed subscriber of the newsletter; `tag` = those with `tag_id`.
    pub target: String,
    #[serde(default)]
    pub tag_id: Option<i64>,
}

/// Adds existing subscribers by hand. People already in it (or who went through it) are skipped.
pub async fn enroll_now(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(i): Json<EnrollInput>,
) -> J {
    let a = automation_for(&st, &u, id).await?;
    let (_, site) = crate::api::list_for(&st, &u, a.list_id).await?;
    let tag = match (i.target.as_str(), i.tag_id) {
        ("all", _) => None,
        ("tag", Some(t)) if t > 0 => {
            check_tag(&mut *st.db.acquire().await?, site.id, t).await?;
            Some(t)
        }
        _ => {
            return Err(AppError::bad_request(
                "target must be all, or tag with a tag_id",
            ))
        }
    };
    let ts = now();
    let mut tx = st.db.begin().await?;
    // Lock + bump so a scheduler pass working from the old flowchart can't interleave.
    bump_revision(&mut tx, id).await?;
    let (steps,): (String,) = sqlx::query_as("SELECT steps FROM automations WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let steps: Vec<Step> = serde_json::from_str(&steps).unwrap_or_default();
    let Some(first) = steps.first().map(|s| s.id.clone()) else {
        return Err(AppError::bad_request("add at least one step first"));
    };
    let r = sqlx::query(
        "INSERT INTO automation_runs (automation_id, subscription_id, status, step_id, entered_at, created_at) \
         SELECT $1, s.id, 'active', $2, $3, $3 FROM subscriptions s JOIN subscribers sb ON sb.id = s.subscriber_id \
         WHERE s.list_id = $4 AND s.status = 'confirmed' AND sb.status = 'active' \
         AND ($5 = 0 OR EXISTS (SELECT 1 FROM subscriber_tags st WHERE st.subscriber_id = s.subscriber_id AND st.tag_id = $5)) \
         ON CONFLICT (automation_id, subscription_id) DO NOTHING",
    )
    .bind(id)
    .bind(first)
    .bind(&ts)
    .bind(a.list_id)
    .bind(tag.unwrap_or(0))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "enrolled": r.rows_affected() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup(steps: &str, trigger: &str) -> (Db, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("lc-test-{}.db", crate::db::random_token(8)));
        let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        for q in [
            "INSERT INTO accounts (name, created_at) VALUES ('a', 'now')",
            "INSERT INTO sites (account_id, name, domain, from_name, from_email, webhook_secret, created_at) \
             VALUES (1, 'S', 'example.com', 'F', 'f@example.com', 's', 'now')",
            "INSERT INTO lists (site_id, public_id, name, created_at) VALUES (1, 'p', 'L', 'now'), (1, 'p2', 'Other', 'now')",
            "INSERT INTO tags (site_id, name, created_at) VALUES (1, 'vip', 'now'), (1, 'buyer', 'now')",
            // Email campaigns 1..3 of automation 1, plus broadcast 4.
            "INSERT INTO automations (list_id, name, status, created_at, started_at) VALUES (1, 'A', 'active', 'now', '2026-01-01T00:00:00Z')",
            "INSERT INTO campaigns (list_id, subject, html_body, status, created_at, automation_id) VALUES \
             (1, 'E1', 'b', 'sending', 'now', 1), (1, 'E2', 'b', 'sending', 'now', 1), (1, 'E3', 'b', 'sending', 'now', 1)",
            "INSERT INTO campaigns (list_id, subject, html_body, status, created_at) VALUES (1, 'News', 'b', 'sent', 'now')",
            "INSERT INTO subscribers (site_id, email, status, created_at) VALUES \
             (1, 'a@x.com', 'active', 'now'), (1, 'b@x.com', 'active', 'now'), (1, 'c@x.com', 'active', 'now')",
            // a: joined after the automation started. b: before it. c: imported.
            "INSERT INTO subscriptions (list_id, subscriber_id, status, token, consent_source, created_at, confirmed_at) VALUES \
             (1, 1, 'confirmed', 't1', 'form', 'now', '2026-02-01T10:00:00Z'), \
             (1, 2, 'confirmed', 't2', 'form', 'now', '2025-12-01T10:00:00Z'), \
             (1, 3, 'confirmed', 't3', 'import', 'now', '2026-02-01T10:00:00Z')",
        ] {
            sqlx::query(q).execute(&db).await.unwrap();
        }
        sqlx::query("UPDATE automations SET steps = $1, trigger_config = $2")
            .bind(steps)
            .bind(trigger)
            .execute(&db)
            .await
            .unwrap();
        (db, path)
    }

    const SUBSCRIBED: &str = r#"{"type":"subscribed"}"#;

    /// Subjects queued for `sub`, in order; marks them sent (standing in for the worker).
    async fn sent(db: &Db, sub: i64) -> Vec<String> {
        sqlx::query("UPDATE campaign_sends SET status = 'sent' WHERE subscription_id = $1 AND status = 'queued'")
            .bind(sub)
            .execute(db)
            .await
            .unwrap();
        sqlx::query_as::<_, (String,)>(
            "SELECT c.subject FROM campaign_sends cs JOIN campaigns c ON c.id = cs.campaign_id \
             WHERE cs.subscription_id = $1 ORDER BY cs.id",
        )
        .bind(sub)
        .fetch_all(db)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.0)
        .collect()
    }

    /// A scheduler run with the sender standing by: whatever gets queued is delivered at once.
    async fn tick(db: &Db, ts: &str) {
        for _ in 0..4 {
            advance(db, ts).await.unwrap();
            sqlx::query("UPDATE campaign_sends SET status = 'sent' WHERE status = 'queued'")
                .execute(db)
                .await
                .unwrap();
        }
    }

    async fn exec(db: &Db, q: &str) {
        sqlx::query(sqlx::AssertSqlSafe(q.to_string()))
            .execute(db)
            .await
            .unwrap();
    }

    async fn run_status(db: &Db, sub: i64) -> (String, String) {
        sqlx::query_as("SELECT status, step_id FROM automation_runs WHERE subscription_id = $1")
            .bind(sub)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn emails_and_waits_on_schedule() {
        let steps = r#"[{"id":"e1","type":"email","campaign_id":1},
            {"id":"w1","type":"wait","amount":1,"unit":"days"},
            {"id":"e2","type":"email","campaign_id":2}]"#;
        let (db, path) = setup(steps, SUBSCRIBED).await;
        tick(&db, "2026-02-01T10:00:30Z").await;
        assert_eq!(sent(&db, 1).await, ["E1"], "first email right away");
        assert!(sent(&db, 2).await.is_empty(), "joined before it started");
        assert!(sent(&db, 3).await.is_empty(), "imported");
        assert_eq!(run_status(&db, 1).await, ("active".into(), "w1".into()));

        tick(&db, "2026-02-02T10:00:00Z").await;
        assert_eq!(sent(&db, 1).await, ["E1"], "not a day yet");
        tick(&db, "2026-02-02T10:00:30Z").await;
        tick(&db, "2026-02-02T10:00:31Z").await;
        assert_eq!(sent(&db, 1).await, ["E1", "E2"]);
        assert_eq!(run_status(&db, 1).await.0, "done");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn imports_can_be_included() {
        let (db, path) = setup(
            r#"[{"id":"e1","type":"email","campaign_id":1}]"#,
            r#"{"type":"subscribed","include_imports":true}"#,
        )
        .await;
        tick(&db, "2026-02-01T10:00:30Z").await;
        assert_eq!(sent(&db, 3).await, ["E1"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn branch_on_tag_and_tag_steps() {
        let steps = r#"[{"id":"t","type":"tag","action":"add","tag_id":2},
            {"id":"b","type":"branch","condition":{"type":"has_tag","tag_id":1},
             "yes":[{"id":"e1","type":"email","campaign_id":1}],
             "no":[{"id":"e2","type":"email","campaign_id":2},{"id":"x","type":"exit"}]},
            {"id":"e3","type":"email","campaign_id":3}]"#;
        let (db, path) = setup(steps, SUBSCRIBED).await;
        // b also joins after it started, with the vip tag.
        exec(
            &db,
            "UPDATE subscriptions SET confirmed_at = '2026-02-01T11:00:00Z' WHERE id = 2",
        )
        .await;
        exec(
            &db,
            "INSERT INTO subscriber_tags (subscriber_id, tag_id, added_at) VALUES (2, 1, 'now')",
        )
        .await;
        tick(&db, "2026-02-01T12:00:00Z").await;
        assert_eq!(sent(&db, 1).await, ["E2"], "no vip tag: no path, then exit");
        assert_eq!(
            sent(&db, 2).await,
            ["E1", "E3"],
            "vip: yes path, then carries on below"
        );
        assert_eq!(run_status(&db, 1).await.0, "done");
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM subscriber_tags WHERE tag_id = 2")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 2, "both got the buyer tag");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn tag_and_click_triggers() {
        let (db, path) = setup(
            r#"[{"id":"e1","type":"email","campaign_id":1}]"#,
            r#"{"type":"tag_added","tag_id":1}"#,
        )
        .await;
        tick(&db, "2026-02-01T12:00:00Z").await;
        assert!(sent(&db, 1).await.is_empty());
        // b got the tag after it started (and lost it again before the scheduler ran); c before.
        exec(&db, "INSERT INTO tag_events (subscriber_id, tag_id, added_at) VALUES (2, 1, '2026-02-02T00:00:00Z'), (3, 1, '2025-01-01T00:00:00Z')").await;
        exec(&db, "INSERT INTO subscriber_tags (subscriber_id, tag_id, added_at) VALUES (3, 1, '2025-01-01T00:00:00Z')").await;
        tick(&db, "2026-02-02T00:00:10Z").await;
        assert_eq!(sent(&db, 2).await, ["E1"], "tagged after it started");
        assert!(sent(&db, 3).await.is_empty(), "tagged before it started");

        exec(&db, r#"UPDATE automations SET trigger_config = '{"type":"clicked","campaign_id":4,"url":"https://x.com/buy"}'"#).await;
        exec(&db, "INSERT INTO links (key, campaign_id, url) VALUES ('k1', 4, 'https://x.com/buy'), ('k2', 4, 'https://x.com/other')").await;
        exec(&db, "INSERT INTO engagements (campaign_id, subscription_id, kind, link_id, created_at) VALUES \
             (4, 1, 'click', 2, '2026-02-03T00:00:00Z'), (4, 3, 'click', 1, '2026-02-03T00:00:00Z')").await;
        tick(&db, "2026-02-03T00:00:10Z").await;
        assert!(sent(&db, 1).await.is_empty(), "clicked a different link");
        assert_eq!(sent(&db, 3).await, ["E1"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn goals_pull_people_ahead_and_wait_until() {
        let steps = r#"[{"id":"e1","type":"email","campaign_id":1},
            {"id":"w","type":"wait","amount":2,"unit":"days","until":{"type":"clicked","campaign_id":1}},
            {"id":"e2","type":"email","campaign_id":2},
            {"id":"w2","type":"wait","amount":7,"unit":"days"},
            {"id":"g","type":"goal","name":"Bought","condition":{"type":"has_tag","tag_id":2},"unmet":"wait"},
            {"id":"e3","type":"email","campaign_id":3}]"#;
        let (db, path) = setup(steps, SUBSCRIBED).await;
        tick(&db, "2026-02-01T10:00:30Z").await;
        assert_eq!(sent(&db, 1).await, ["E1"]);
        // Clicks E1 an hour later: the wait ends early.
        exec(&db, "INSERT INTO engagements (campaign_id, subscription_id, kind, created_at) VALUES (1, 1, 'click', 'now')").await;
        tick(&db, "2026-02-01T11:00:00Z").await;
        assert_eq!(sent(&db, 1).await, ["E1", "E2"]);
        assert_eq!(run_status(&db, 1).await.1, "w2");
        // Gets the buyer tag mid-wait: jumps past the goal.
        exec(
            &db,
            "INSERT INTO subscriber_tags (subscriber_id, tag_id, added_at) VALUES (1, 2, 'now')",
        )
        .await;
        tick(&db, "2026-02-01T12:00:00Z").await;
        assert_eq!(sent(&db, 1).await, ["E1", "E2", "E3"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn unmet_goal_waits_and_unwritten_or_paused_emails_hold() {
        let steps = r#"[{"id":"g","type":"goal","condition":{"type":"has_tag","tag_id":1},"unmet":"wait"},
            {"id":"e1","type":"email","campaign_id":1}]"#;
        let (db, path) = setup(steps, SUBSCRIBED).await;
        exec(&db, "UPDATE campaigns SET html_body = '' WHERE id = 1").await;
        tick(&db, "2026-02-01T10:00:30Z").await;
        assert_eq!(run_status(&db, 1).await.1, "g", "held at the unmet goal");
        exec(
            &db,
            "INSERT INTO subscriber_tags (subscriber_id, tag_id, added_at) VALUES (1, 1, 'now')",
        )
        .await;
        tick(&db, "2026-02-01T10:01:00Z").await;
        assert_eq!(
            run_status(&db, 1).await.1,
            "e1",
            "held at the unwritten email"
        );
        exec(
            &db,
            "UPDATE campaigns SET html_body = 'b', status = 'paused' WHERE id = 1",
        )
        .await;
        tick(&db, "2026-02-01T10:02:00Z").await;
        assert!(sent(&db, 1).await.is_empty(), "paused email holds too");
        exec(&db, "UPDATE campaigns SET status = 'sending' WHERE id = 1").await;
        tick(&db, "2026-02-01T10:03:00Z").await;
        assert_eq!(sent(&db, 1).await, ["E1"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn unsubscribed_people_are_held_and_lists_steps() {
        let steps = r#"[{"id":"w","type":"wait","amount":1,"unit":"hours"},
            {"id":"l","type":"list","action":"subscribe","list_id":2},
            {"id":"u","type":"list","action":"unsubscribe","list_id":1},
            {"id":"e1","type":"email","campaign_id":1}]"#;
        let (db, path) = setup(steps, SUBSCRIBED).await;
        tick(&db, "2026-02-01T10:00:30Z").await;
        exec(
            &db,
            "UPDATE subscriptions SET status = 'unsubscribed' WHERE id = 1",
        )
        .await;
        tick(&db, "2026-02-01T12:00:00Z").await;
        assert_eq!(run_status(&db, 1).await.1, "w", "held while unsubscribed");
        exec(
            &db,
            "UPDATE subscriptions SET status = 'confirmed' WHERE id = 1",
        )
        .await;
        tick(&db, "2026-02-01T12:00:10Z").await;
        let (s,): (String,) = sqlx::query_as(
            "SELECT status FROM subscriptions WHERE list_id = 2 AND subscriber_id = 1",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(s, "confirmed", "moved to the other newsletter");
        let (s,): (String,) = sqlx::query_as("SELECT status FROM subscriptions WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(s, "unsubscribed");
        assert_eq!(
            run_status(&db, 1).await.0,
            "done",
            "leaving this newsletter ends it"
        );
        assert!(sent(&db, 1).await.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn stale_decisions_are_dropped() {
        let (db, path) = setup(
            r#"[{"id":"e1","type":"email","campaign_id":1}]"#,
            SUBSCRIBED,
        )
        .await;
        exec(&db, "UPDATE automations SET revision = 5").await;
        let a: AutomationRow = sqlx::query_as(
            "SELECT id, list_id, trigger_config, steps, started_at, revision FROM automations",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        // Edited after it was read: nothing happens.
        exec(&db, "UPDATE automations SET revision = 6").await;
        assert_eq!(
            advance_one(&db, &a, "2026-02-01T10:00:30Z").await.unwrap(),
            0
        );
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM automation_runs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(advance(&db, "2026-02-01T10:00:30Z").await.unwrap() > 0);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn emails_go_out_before_later_steps_and_skipped_ones_come_back() {
        let steps = r#"[{"id":"e1","type":"email","campaign_id":1},
            {"id":"e2","type":"email","campaign_id":2},
            {"id":"u","type":"list","action":"unsubscribe","list_id":1}]"#;
        let (db, path) = setup(steps, SUBSCRIBED).await;
        advance(&db, "2026-02-01T10:00:30Z").await.unwrap();
        advance(&db, "2026-02-01T10:00:31Z").await.unwrap();
        assert_eq!(
            run_status(&db, 1).await.1,
            "e1",
            "waits for E1 to be sent before E2"
        );
        // E1 is skipped (they unsubscribed before it went out), then they come back.
        exec(
            &db,
            "UPDATE subscriptions SET status = 'unsubscribed' WHERE id = 1",
        )
        .await;
        exec(&db, "UPDATE campaign_sends SET status = 'skipped'").await;
        advance(&db, "2026-02-02T00:00:00Z").await.unwrap();
        exec(
            &db,
            "UPDATE subscriptions SET status = 'confirmed' WHERE id = 1",
        )
        .await;
        tick(&db, "2026-02-03T00:00:00Z").await;
        assert_eq!(
            sent(&db, 1).await,
            ["E1", "E2"],
            "E1 restored, and E2 sent before the unsubscribe step"
        );
        let (s,): (String,) = sqlx::query_as("SELECT status FROM subscriptions WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(s, "unsubscribed");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn incomplete_steps_hold_people() {
        let steps = r#"[{"id":"t","type":"tag","action":"add","tag_id":0},{"id":"e1","type":"email","campaign_id":1}]"#;
        let (db, path) = setup(steps, SUBSCRIBED).await;
        tick(&db, "2026-02-01T10:00:30Z").await;
        assert_eq!(run_status(&db, 1).await.1, "t");
        assert!(sent(&db, 1).await.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn late_catch_up_keeps_the_schedule() {
        let steps = r#"[{"id":"e1","type":"email","campaign_id":1},
            {"id":"w1","type":"wait","amount":2,"unit":"days"},
            {"id":"e2","type":"email","campaign_id":2},
            {"id":"w2","type":"wait","amount":2,"unit":"days"},
            {"id":"e3","type":"email","campaign_id":3}]"#;
        let (db, path) = setup(steps, SUBSCRIBED).await;
        tick(&db, "2026-02-01T10:00:30Z").await;
        // Paused until day 3: E2 (due day 2) goes then, but E3 stays due on day 4, not day 5.
        exec(&db, "UPDATE automations SET status = 'paused'").await;
        tick(&db, "2026-02-03T12:00:00Z").await;
        exec(&db, "UPDATE automations SET status = 'active'").await;
        tick(&db, "2026-02-04T00:00:00Z").await;
        assert_eq!(sent(&db, 1).await, ["E1", "E2"]);
        tick(&db, "2026-02-05T10:00:30Z").await;
        assert_eq!(sent(&db, 1).await, ["E1", "E2", "E3"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn goal_jump_cancels_an_unsent_email_and_released_goals_start_now() {
        let steps = r#"[{"id":"e1","type":"email","campaign_id":1},
            {"id":"g","type":"goal","condition":{"type":"has_tag","tag_id":1},"unmet":"wait"},
            {"id":"w","type":"wait","amount":1,"unit":"days"},
            {"id":"e2","type":"email","campaign_id":2}]"#;
        let (db, path) = setup(steps, SUBSCRIBED).await;
        advance(&db, "2026-02-01T10:00:30Z").await.unwrap();
        // E1 is retrying when they meet the goal: it's dropped, not sent after all.
        exec(
            &db,
            "INSERT INTO subscriber_tags (subscriber_id, tag_id, added_at) VALUES (1, 1, 'now')",
        )
        .await;
        advance(&db, "2026-02-08T00:00:00Z").await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM campaign_sends")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(run_status(&db, 1).await.1, "w");

        // b reaches the goal and waits there a week; the day's wait after it starts on release.
        exec(
            &db,
            "UPDATE subscriptions SET confirmed_at = '2026-02-08T00:00:00Z' WHERE id = 2",
        )
        .await;
        tick(&db, "2026-02-08T00:00:10Z").await;
        assert_eq!(run_status(&db, 2).await.1, "g");
        exec(
            &db,
            "INSERT INTO subscriber_tags (subscriber_id, tag_id, added_at) VALUES (2, 1, 'now')",
        )
        .await;
        tick(&db, "2026-02-15T00:00:00Z").await;
        assert_eq!(
            run_status(&db, 2).await.1,
            "w",
            "a full day's wait still ahead"
        );
        tick(&db, "2026-02-16T00:00:00Z").await;
        assert_eq!(sent(&db, 2).await, ["E1", "E2"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn paused_automation_does_nothing() {
        let (db, path) = setup(
            r#"[{"id":"e1","type":"email","campaign_id":1}]"#,
            SUBSCRIBED,
        )
        .await;
        exec(&db, "UPDATE automations SET status = 'paused'").await;
        tick(&db, "2026-03-01T00:00:00Z").await;
        assert!(sent(&db, 1).await.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn goals_only_on_the_path_ahead() {
        let steps: Vec<Step> = serde_json::from_str(
            r#"[{"id":"b","type":"branch","condition":{"type":"has_tag","tag_id":1},
                 "yes":[{"id":"y1","type":"exit"},{"id":"gy","type":"goal","condition":{"type":"has_tag","tag_id":1}}],
                 "no":[{"id":"n1","type":"exit"},{"id":"gn","type":"goal","condition":{"type":"has_tag","tag_id":1}}]},
                {"id":"g","type":"goal","condition":{"type":"has_tag","tag_id":1}}]"#,
        )
        .unwrap();
        let plan = Plan::new(&steps);
        let ids = |v: Vec<&Step>| v.into_iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(plan.goals_after("y1")),
            ["gy", "g"],
            "not the No arm's goal"
        );
        assert_eq!(
            ids(plan.goals_after("n1")),
            ["gn", "g"],
            "not the Yes arm's goal"
        );
        assert_eq!(ids(plan.goals_after("b")), ["gy", "gn", "g"]);
    }

    #[test]
    fn removed_steps_remap_to_what_came_after() {
        let old: Vec<Step> = serde_json::from_str(
            r#"[{"id":"a","type":"exit"},
                {"id":"b","type":"branch","condition":{"type":"has_tag","tag_id":1},
                 "yes":[{"id":"y1","type":"exit"},{"id":"y2","type":"exit"}],"no":[]},
                {"id":"c","type":"exit"}]"#,
        )
        .unwrap();
        let plan = Plan::new(&old);
        let ids: HashSet<&str> = ["a", "y2", "c"].into_iter().collect();
        assert_eq!(remap(&plan, &ids, "y1").as_deref(), Some("y2"));
        assert_eq!(
            remap(&plan, &ids, "b").as_deref(),
            Some("c"),
            "a removed branch continues below it"
        );
        let ids: HashSet<&str> = ["a"].into_iter().collect();
        assert_eq!(
            remap(&plan, &ids, "y2"),
            None,
            "nothing left after it: done"
        );
        assert_eq!(plan.after("y2"), Some("c"));
        assert_eq!(plan.after("c"), None);
    }

    #[tokio::test]
    async fn converting_a_reordered_course_keeps_the_lesson_in_flight() {
        let (db, path) = setup("[]", SUBSCRIBED).await;
        exec(&db, "DELETE FROM automations").await;
        exec(&db, "INSERT INTO sequences (list_id, name, status, created_at, started_at) VALUES (1, 'Course', 'active', 'now', '2026-01-01T00:00:00Z')").await;
        // L1 was sent, then moved after L0, whose send is still being retried.
        exec(&db, "INSERT INTO campaigns (list_id, subject, html_body, status, created_at, sequence_id, delay_days) VALUES \
             (1, 'L0', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 0), (1, 'L1', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 5)").await;
        exec(&db, "INSERT INTO sequence_enrollments (sequence_id, subscription_id, enrolled_at) VALUES (1, 1, '2026-02-01T10:00:00Z')").await;
        exec(&db, "INSERT INTO campaign_sends (campaign_id, subscription_id, status) VALUES (5, 1, 'queued'), (6, 1, 'sent')").await;
        convert_courses(&db).await.unwrap();
        let (step, steps): (String, String) = sqlx::query_as(
            "SELECT r.step_id, a.steps FROM automation_runs r JOIN automations a ON a.id = r.automation_id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let steps: Vec<Step> = serde_json::from_str(&steps).unwrap();
        assert_eq!(step, steps[0].id, "at L0, which is still being sent");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn converting_a_reordered_course_keeps_an_unsent_lesson() {
        let (db, path) = setup("[]", SUBSCRIBED).await;
        exec(&db, "DELETE FROM automations").await;
        exec(&db, "INSERT INTO sequences (list_id, name, status, created_at, started_at) VALUES (1, 'Course', 'active', 'now', '2026-01-01T00:00:00Z')").await;
        // L1 was delivered, then moved after L2, which hasn't gone out yet.
        exec(&db, "INSERT INTO campaigns (list_id, subject, html_body, status, created_at, sequence_id, delay_days) VALUES \
             (1, 'L0', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 0), (1, 'L2', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 2), \
             (1, 'L1', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 5)").await;
        exec(&db, "INSERT INTO sequence_enrollments (sequence_id, subscription_id, enrolled_at) VALUES (1, 1, '2026-02-01T10:00:00Z')").await;
        exec(&db, "INSERT INTO campaign_sends (campaign_id, subscription_id, status) VALUES (5, 1, 'sent'), (7, 1, 'sent')").await;
        convert_courses(&db).await.unwrap();
        tick(&db, "2026-02-03T10:00:00Z").await;
        tick(&db, "2026-02-09T10:00:00Z").await;
        assert_eq!(
            sent(&db, 1).await,
            ["L0", "L1", "L2"],
            "L2 still sent, L1 not again"
        );
        assert_eq!(run_status(&db, 1).await.0, "done");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn converting_waits_for_a_later_lesson_in_flight() {
        let (db, path) = setup("[]", SUBSCRIBED).await;
        exec(&db, "DELETE FROM automations").await;
        exec(&db, "INSERT INTO sequences (list_id, name, status, created_at, started_at) VALUES (1, 'Course', 'active', 'now', '2026-01-01T00:00:00Z')").await;
        exec(&db, "INSERT INTO campaigns (list_id, subject, html_body, status, created_at, sequence_id, delay_days) VALUES \
             (1, 'L0', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 0), (1, 'L1', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 1)").await;
        exec(&db, "INSERT INTO sequence_enrollments (sequence_id, subscription_id, enrolled_at) VALUES (1, 1, '2026-02-01T10:00:00Z')").await;
        // L1 is being retried; L0 (moved before it) hasn't been sent.
        exec(&db, "INSERT INTO campaign_sends (campaign_id, subscription_id, status) VALUES (6, 1, 'queued')").await;
        convert_courses(&db).await.unwrap();
        let (step, steps): (String, String) = sqlx::query_as(
            "SELECT r.step_id, a.steps FROM automation_runs r JOIN automations a ON a.id = r.automation_id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let steps: Vec<Step> = serde_json::from_str(&steps).unwrap();
        assert_eq!(steps[2].kind, StepKind::Email { campaign_id: 6 });
        assert_eq!(step, steps[2].id, "at L1, still being sent");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn link_rules_are_normalized_like_tracked_links() {
        let mut t = Trigger::Clicked {
            campaign_id: 1,
            url: " https://Example.com ".into(),
        };
        normalize_trigger(&mut t).unwrap();
        assert_eq!(
            t,
            Trigger::Clicked {
                campaign_id: 1,
                url: "https://example.com/".into()
            }
        );
        let mut steps: Vec<Step> = serde_json::from_str(
            r#"[{"id":"b","type":"branch","condition":{"type":"clicked","campaign_id":1,"url":"javascript:alert(1)"}}]"#,
        )
        .unwrap();
        assert!(normalize_steps(&mut steps).is_err());
    }

    #[tokio::test]
    async fn courses_are_converted() {
        let (db, path) = setup("[]", SUBSCRIBED).await;
        exec(&db, "DELETE FROM automations").await;
        exec(&db, "INSERT INTO sequences (list_id, name, status, created_at, started_at) VALUES (1, 'Course', 'active', 'now', '2026-01-01T00:00:00Z')").await;
        exec(&db, "INSERT INTO campaigns (list_id, subject, html_body, status, created_at, sequence_id, delay_days) VALUES \
             (1, 'L0', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 0), (1, 'L2', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 2)").await;
        exec(&db, "INSERT INTO sequence_enrollments (sequence_id, subscription_id, enrolled_at) VALUES (1, 1, '2026-02-01T10:00:00Z')").await;
        exec(&db, "INSERT INTO campaign_sends (campaign_id, subscription_id, status) VALUES (5, 1, 'sent')").await;
        // Another subscriber's day-0 lesson is still being retried.
        exec(
            &db,
            "UPDATE subscriptions SET confirmed_at = '2026-02-01T10:00:00Z' WHERE id = 2",
        )
        .await;
        exec(&db, "INSERT INTO sequence_enrollments (sequence_id, subscription_id, enrolled_at) VALUES (1, 2, '2026-02-01T10:00:00Z')").await;
        exec(&db, "INSERT INTO campaign_sends (campaign_id, subscription_id, status) VALUES (5, 2, 'queued')").await;
        convert_courses(&db).await.unwrap();
        convert_courses(&db).await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM automations")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let (step,): (String,) =
            sqlx::query_as("SELECT step_id FROM automation_runs WHERE subscription_id = 2")
                .fetch_one(&db)
                .await
                .unwrap();
        let steps: (String,) = sqlx::query_as("SELECT steps FROM automations")
            .fetch_one(&db)
            .await
            .unwrap();
        let steps: Vec<Step> = serde_json::from_str(&steps.0).unwrap();
        assert_eq!(step, steps[0].id, "held at the lesson still being sent");
        // Day 0 sent; carries on to day 2 at the original time.
        tick(&db, "2026-02-03T09:59:00Z").await;
        assert_eq!(sent(&db, 1).await, ["L0"]);
        tick(&db, "2026-02-03T10:00:00Z").await;
        tick(&db, "2026-02-03T10:00:01Z").await;
        assert_eq!(sent(&db, 1).await, ["L0", "L2"]);
        let _ = std::fs::remove_file(path);
    }
}
