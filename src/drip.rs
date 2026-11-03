//! Email courses (drip sequences): a series of lessons sent to each subscriber on day N after they
//! confirm. A lesson is a campaign row with `sequence_id` set; this module only decides *when* a
//! subscriber is due a lesson and queues a `campaign_sends` row for it. The normal sender
//! (worker.rs) delivers it with the usual claiming, retries, rate limits, unsubscribe checks and
//! compliance footer, and only while the course is active.
//!
//! Who is enrolled: subscribers whose subscription is confirmed at or after the moment the course
//! was first started, except CSV imports (existing readers shouldn't all get lesson 1 at once). The
//! schedule counts from the confirmation time, so lessons arrive at the time of day the person
//! signed up.
//!
//! Progress is derived from what each subscriber has actually been sent, lesson by lesson in
//! (delay_days, id) order, with at most one lesson in flight per subscriber: the next lesson is
//! queued only once the previous one was sent (or definitively failed). So lessons never overtake a
//! retrying or paused one, editing a lesson's day can't skip anyone, and a lesson skipped while the
//! person was unsubscribed is restored when they resubscribe. A lesson added to a running course
//! reaches only people whose day for it is still ahead.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthUser;
use crate::db::{now, Db};
use crate::error::{AppError, AppResult};
use crate::models::{Campaign, CAMPAIGN_COLS};
use crate::AppState;

type J = AppResult<Json<Value>>;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Sequence {
    pub id: i64,
    pub list_id: i64,
    pub name: String,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
}

const SEQUENCE_COLS: &str = "id, list_id, name, status, created_at, started_at";

fn plus_days(ts: &str, days: i64) -> Option<String> {
    let t = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    Some(
        (t.with_timezone(&chrono::Utc) + chrono::Duration::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

#[derive(Debug, Clone)]
struct Lesson {
    id: i64,
    delay_days: i64,
    /// `sending` = live; `paused` = stopped after repeated failures.
    live: bool,
    /// Has a subject and body.
    written: bool,
    created_at: String,
}

#[derive(Debug, PartialEq)]
enum Next {
    /// Nothing to do yet.
    Wait,
    /// Queue this lesson.
    Queue(i64),
    /// Re-queue this lesson's send, skipped while the subscriber couldn't receive mail.
    Restore(i64),
    /// Every lesson is behind them.
    Done,
}

/// What one (currently mailable) subscriber needs next. `sends` maps lesson id → send status.
fn next_step(
    lessons: &[Lesson],
    sends: &HashMap<i64, String>,
    enrolled_at: &str,
    now_ts: &str,
) -> Next {
    // One lesson in flight at a time, wherever it sits in the (possibly re-ordered) course.
    if sends.values().any(|s| s == "queued" || s == "sending") {
        return Next::Wait;
    }
    for l in lessons {
        let status = sends.get(&l.id).map(String::as_str);
        // Delivered, or retries exhausted / delivery unknown (may have arrived): behind them,
        // whatever happens to the lesson later.
        if matches!(status, Some("sent") | Some("failed")) {
            continue;
        }
        let Some(due) = plus_days(enrolled_at, l.delay_days) else {
            return Next::Wait;
        };
        // Added to the course after this person's day for it had passed.
        if status.is_none() && due < l.created_at {
            continue;
        }
        // A paused or unwritten lesson holds the course here.
        if !l.live || !l.written {
            return Next::Wait;
        }
        match status {
            _ if due.as_str() > now_ts => return Next::Wait,
            Some("skipped") => return Next::Restore(l.id),
            Some(_) => return Next::Wait,
            None => return Next::Queue(l.id),
        }
    }
    Next::Done
}

/// Enrolls newly eligible subscribers and queues each one's next lesson when it's due.
/// Idempotent: enrollment and send rows are insert-if-absent and restores only touch skipped rows.
pub async fn advance(db: &Db, now_ts: &str) -> anyhow::Result<usize> {
    sqlx::query(
        "INSERT INTO sequence_enrollments (sequence_id, subscription_id, enrolled_at) \
         SELECT q.id, s.id, s.confirmed_at FROM sequences q \
         JOIN subscriptions s ON s.list_id = q.list_id \
         JOIN subscribers sb ON sb.id = s.subscriber_id \
         WHERE q.status = 'active' AND s.status = 'confirmed' AND sb.status = 'active' \
           AND s.consent_source <> 'import' AND s.confirmed_at IS NOT NULL AND s.confirmed_at >= q.started_at \
           AND NOT EXISTS (SELECT 1 FROM sequence_enrollments e WHERE e.sequence_id = q.id AND e.subscription_id = s.id) \
         ON CONFLICT (sequence_id, subscription_id) DO NOTHING",
    )
    .execute(db)
    .await?;

    // `revision` is bumped (in the same transaction) by every lesson add, edit, delete or resume.
    // Each write below only applies if it's unchanged since we read it here, before the lessons,
    // so a decision made from a course that has since been edited is dropped and remade next run.
    let seqs: Vec<(i64, i64)> =
        sqlx::query_as("SELECT id, revision FROM sequences WHERE status = 'active'")
            .fetch_all(db)
            .await?;
    let mut changed = 0;
    for (seq_id, rev) in seqs {
        let lessons: Vec<Lesson> = sqlx::query_as::<_, (i64, i64, String, String, String, String)>(
            "SELECT id, delay_days, status, subject, html_body, created_at FROM campaigns \
             WHERE sequence_id = $1 ORDER BY delay_days, id",
        )
        .bind(seq_id)
        .fetch_all(db)
        .await?
        .into_iter()
        .map(
            |(id, delay_days, status, subject, body, created_at)| Lesson {
                id,
                delay_days,
                live: status == "sending",
                written: !subject.trim().is_empty() && !body.trim().is_empty(),
                created_at,
            },
        )
        .collect();
        // Only subscribers who can receive mail right now; the rest wait (and resume later).
        let enrolled: Vec<(i64, String)> = sqlx::query_as(
            "SELECT e.subscription_id, e.enrolled_at FROM sequence_enrollments e \
             JOIN subscriptions s ON s.id = e.subscription_id JOIN subscribers sb ON sb.id = s.subscriber_id \
             WHERE e.sequence_id = $1 AND e.status = 'active' AND s.status = 'confirmed' AND sb.status = 'active'",
        )
        .bind(seq_id)
        .fetch_all(db)
        .await?;
        if enrolled.is_empty() {
            continue;
        }
        let mut sends: HashMap<i64, HashMap<i64, String>> = HashMap::new();
        let rows: Vec<(i64, i64, String)> = sqlx::query_as(
            "SELECT cs.subscription_id, cs.campaign_id, cs.status FROM campaign_sends cs \
             JOIN campaigns c ON c.id = cs.campaign_id \
             JOIN sequence_enrollments e ON e.sequence_id = c.sequence_id AND e.subscription_id = cs.subscription_id \
             WHERE c.sequence_id = $1 AND e.status = 'active'",
        )
        .bind(seq_id)
        .fetch_all(db)
        .await?;
        for (sub, cid, status) in rows {
            sends.entry(sub).or_default().insert(cid, status);
        }
        let none = HashMap::new();
        for (sub_id, enrolled_at) in enrolled {
            let mine = sends.get(&sub_id).unwrap_or(&none);
            let step = next_step(&lessons, mine, &enrolled_at, now_ts);
            if step == Next::Wait {
                continue;
            }
            // Lock the course row and re-check its revision before writing. An edit bumps the
            // revision first thing in its transaction, so this waits for any edit in progress
            // (Postgres row lock; SQLite serializes writers) and then sees whether it changed.
            let mut tx = db.begin().await?;
            let locked = sqlx::query(
                "UPDATE sequences SET revision = revision WHERE id = $1 AND revision = $2",
            )
            .bind(seq_id)
            .bind(rev)
            .execute(&mut *tx)
            .await?;
            if locked.rows_affected() == 0 {
                break; // course changed since we read it; re-decide on the next run
            }
            let r = match step {
                Next::Wait => unreachable!(),
                Next::Queue(cid) => {
                    sqlx::query(
                        "INSERT INTO campaign_sends (campaign_id, subscription_id, status) VALUES ($1, $2, 'queued') \
                         ON CONFLICT (campaign_id, subscription_id) DO NOTHING",
                    )
                    .bind(cid)
                    .bind(sub_id)
                    .execute(&mut *tx)
                    .await?
                }
                Next::Restore(cid) => {
                    sqlx::query(
                        "UPDATE campaign_sends SET status = 'queued', attempts = 0, error = '', next_attempt_at = NULL, sent_at = NULL \
                         WHERE campaign_id = $1 AND subscription_id = $2 AND status = 'skipped'",
                    )
                    .bind(cid)
                    .bind(sub_id)
                    .execute(&mut *tx)
                    .await?
                }
                // Completed (reopened if a lesson is later added or moved; see `reopen`).
                Next::Done => {
                    sqlx::query(
                        "UPDATE sequence_enrollments SET status = 'done' WHERE sequence_id = $1 AND subscription_id = $2 AND status = 'active'",
                    )
                    .bind(seq_id)
                    .bind(sub_id)
                    .execute(&mut *tx)
                    .await?
                }
            };
            tx.commit().await?;
            changed += r.rows_affected() as usize;
        }
    }
    Ok(changed)
}

// ---------------------------------------------------------------------------------------
// API

async fn sequence_for(st: &AppState, u: &AuthUser, id: i64) -> AppResult<Sequence> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT q.{} FROM sequences q JOIN lists l ON l.id = q.list_id JOIN sites s ON s.id = l.site_id \
         WHERE q.id = $1 AND s.account_id = $2",
        SEQUENCE_COLS.replace(", ", ", q.")
    )))
    .bind(id)
    .bind(u.account_id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(AppError::not_found)
}

async fn sequence_json(st: &AppState, q: &Sequence) -> AppResult<Value> {
    let lessons: Vec<Campaign> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CAMPAIGN_COLS} FROM campaigns WHERE sequence_id = $1 ORDER BY delay_days, id"
    )))
    .bind(q.id)
    .fetch_all(&st.db)
    .await?;
    let mut ls = Vec::with_capacity(lessons.len());
    for c in &lessons {
        ls.push(crate::api::campaign_json(st, c).await?);
    }
    let (enrolled, done): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(CASE WHEN status = 'done' THEN 1 END) FROM sequence_enrollments WHERE sequence_id = $1",
    )
    .bind(q.id)
    .fetch_one(&st.db)
    .await?;
    let mut v = serde_json::to_value(q).unwrap();
    v["lessons"] = Value::Array(ls);
    v["stats"] = json!({ "enrolled": enrolled, "completed": done, "in_progress": enrolled - done });
    Ok(v)
}

pub async fn list(State(st): State<AppState>, u: AuthUser, Path(list_id): Path<i64>) -> J {
    crate::api::list_for(&st, &u, list_id).await?;
    let qs: Vec<Sequence> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SEQUENCE_COLS} FROM sequences WHERE list_id = $1 ORDER BY id"
    )))
    .bind(list_id)
    .fetch_all(&st.db)
    .await?;
    let mut out = Vec::with_capacity(qs.len());
    for q in &qs {
        out.push(sequence_json(&st, q).await?);
    }
    Ok(Json(Value::Array(out)))
}

#[derive(Deserialize)]
pub struct SequenceInput {
    #[serde(default)]
    pub name: String,
}

pub async fn create(
    State(st): State<AppState>,
    u: AuthUser,
    Path(list_id): Path<i64>,
    Json(i): Json<SequenceInput>,
) -> J {
    crate::api::list_for(&st, &u, list_id).await?;
    let name = match i.name.trim() {
        "" => "Email course",
        n => n,
    };
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO sequences (list_id, name, status, created_at) VALUES ($1, $2, 'draft', $3) RETURNING id",
    )
    .bind(list_id)
    .bind(name)
    .bind(now())
    .fetch_one(&st.db)
    .await?;
    get(State(st), u, Path(id)).await
}

pub async fn get(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    let q = sequence_for(&st, &u, id).await?;
    Ok(Json(sequence_json(&st, &q).await?))
}

#[derive(Deserialize)]
pub struct SequenceUpdate {
    pub name: Option<String>,
    /// `active` starts (or resumes) the course; `paused` stops new lessons going out.
    pub status: Option<String>,
}

pub async fn update(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(i): Json<SequenceUpdate>,
) -> J {
    let q = sequence_for(&st, &u, id).await?;
    if let Some(name) = i.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        sqlx::query("UPDATE sequences SET name = $1 WHERE id = $2")
            .bind(name)
            .bind(id)
            .execute(&st.db)
            .await?;
    }
    match i.status.as_deref() {
        None => {}
        Some("active") => {
            let (_, site) = crate::api::list_for(&st, &u, q.list_id).await?;
            if site.physical_address.trim().is_empty() {
                return Err(AppError::bad_request(
                    "set a physical mailing address on the site first (required by CAN-SPAM and most providers)",
                ));
            }
            crate::mailer::provider_for(&site, &st.http)
                .map_err(|e| AppError::bad_request(format!("{e:#}")))?;
            let (ready,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM campaigns WHERE sequence_id = $1 AND TRIM(subject) <> '' AND TRIM(html_body) <> ''",
            )
            .bind(id)
            .fetch_one(&st.db)
            .await?;
            if ready == 0 {
                return Err(AppError::bad_request(
                    "write at least one lesson (subject and body) before starting the course",
                ));
            }
            sqlx::query(
                "UPDATE sequences SET status = 'active', started_at = COALESCE(started_at, $1) WHERE id = $2",
            )
            .bind(now())
            .bind(id)
            .execute(&st.db)
            .await?;
            // Starting again also resumes lessons that paused themselves after send failures
            // (e.g. because the provider wasn't set up yet).
            let mut tx = st.db.begin().await?;
            bump_revision(&mut tx, id).await?;
            sqlx::query("UPDATE campaigns SET status = 'sending' WHERE sequence_id = $1 AND status = 'paused'")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        Some("paused") => {
            sqlx::query(
                "UPDATE sequences SET status = 'paused' WHERE id = $1 AND status = 'active'",
            )
            .bind(id)
            .execute(&st.db)
            .await?;
        }
        Some(_) => return Err(AppError::bad_request("status must be active or paused")),
    }
    get(State(st), u, Path(id)).await
}

pub async fn delete(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> J {
    sequence_for(&st, &u, id).await?;
    // Lessons, their queued sends and enrollments go with it (ON DELETE CASCADE).
    sqlx::query("DELETE FROM campaigns WHERE sequence_id = $1")
        .bind(id)
        .execute(&st.db)
        .await?;
    sqlx::query("DELETE FROM sequences WHERE id = $1")
        .bind(id)
        .execute(&st.db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct LessonInput {
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub delay_days: Option<i64>,
}

pub const MAX_DELAY_DAYS: i64 = 3650;

/// After a lesson is added or moved to another day, people who had completed the course are
/// looked at again: those whose day for it is still ahead get it, the rest complete again.
pub async fn reopen(tx: &mut sqlx::Transaction<'_, sqlx::Any>, sequence_id: i64) -> AppResult<()> {
    sqlx::query(
        "UPDATE sequence_enrollments SET status = 'active' WHERE sequence_id = $1 AND status = 'done'",
    )
    .bind(sequence_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Marks the course as changed; see `advance`. Call inside the transaction making the change.
pub async fn bump_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    sequence_id: i64,
) -> AppResult<()> {
    sqlx::query("UPDATE sequences SET revision = revision + 1 WHERE id = $1")
        .bind(sequence_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Adds a lesson. Without `delay_days` it goes one day after the current last lesson (day 0 first).
pub async fn add_lesson(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    Json(i): Json<LessonInput>,
) -> J {
    let q = sequence_for(&st, &u, id).await?;
    let delay = match i.delay_days {
        Some(d) => d,
        None => {
            let (max,): (Option<i64>,) =
                sqlx::query_as("SELECT MAX(delay_days) FROM campaigns WHERE sequence_id = $1")
                    .bind(id)
                    .fetch_one(&st.db)
                    .await?;
            max.map_or(0, |m| m + 1)
        }
    }
    .clamp(0, MAX_DELAY_DAYS);
    // Lessons are always 'sending': the worker delivers whatever the scheduler queues for them.
    let mut tx = st.db.begin().await?;
    bump_revision(&mut tx, id).await?;
    reopen(&mut tx, id).await?;
    let (cid,): (i64,) = sqlx::query_as(
        "INSERT INTO campaigns (list_id, subject, status, created_at, started_at, sequence_id, delay_days) \
         VALUES ($1, $2, 'sending', $3, $3, $4, $5) RETURNING id",
    )
    .bind(q.list_id)
    .bind(i.subject.trim())
    .bind(now())
    .bind(id)
    .bind(delay)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::api::get_campaign(State(st), u, Path(cid)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (Db, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("lc-test-{}.db", crate::db::random_token(8)));
        let db = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        for q in [
            "INSERT INTO accounts (name, created_at) VALUES ('a', 'now')",
            "INSERT INTO sites (account_id, name, domain, from_name, from_email, webhook_secret, created_at) \
             VALUES (1, 'S', 'example.com', 'F', 'f@example.com', 's', 'now')",
            "INSERT INTO lists (site_id, public_id, name, created_at) VALUES (1, 'p', 'L', 'now')",
            "INSERT INTO sequences (list_id, name, status, created_at, started_at) \
             VALUES (1, 'C', 'active', 'now', '2026-01-01T00:00:00Z')",
            // Lessons on day 0, 1 and 2.
            "INSERT INTO campaigns (list_id, subject, html_body, status, created_at, sequence_id, delay_days) VALUES \
             (1, 'L0', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 0), (1, 'L1', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 1), (1, 'L2', 'b', 'sending', '2026-01-01T00:00:00Z', 1, 2)",
            "INSERT INTO subscribers (site_id, email, status, created_at) VALUES \
             (1, 'a@x.com', 'active', 'now'), (1, 'b@x.com', 'active', 'now'), (1, 'c@x.com', 'active', 'now')",
            // a: signed up after the course started. b: before it. c: imported.
            "INSERT INTO subscriptions (list_id, subscriber_id, status, token, consent_source, created_at, confirmed_at) VALUES \
             (1, 1, 'confirmed', 't1', 'form', 'now', '2026-02-01T10:00:00Z'), \
             (1, 2, 'confirmed', 't2', 'form', 'now', '2025-12-01T10:00:00Z'), \
             (1, 3, 'confirmed', 't3', 'import', 'now', '2026-02-01T10:00:00Z')",
        ] {
            sqlx::query(q).execute(&db).await.unwrap();
        }
        (db, path)
    }

    /// Lessons queued for `sub`, in course order; marks them delivered (standing in for the worker).
    async fn deliver(db: &Db, sub: i64) -> Vec<String> {
        sqlx::query("UPDATE campaign_sends SET status = 'sent' WHERE subscription_id = $1 AND status = 'queued'")
            .bind(sub)
            .execute(db)
            .await
            .unwrap();
        sqlx::query_as::<_, (String,)>(
            "SELECT c.subject FROM campaign_sends cs JOIN campaigns c ON c.id = cs.campaign_id \
             WHERE cs.subscription_id = $1 AND cs.status = 'sent' ORDER BY c.delay_days, c.id",
        )
        .bind(sub)
        .fetch_all(db)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.0)
        .collect()
    }

    async fn exec(db: &Db, q: &'static str) {
        sqlx::query(q).execute(db).await.unwrap();
    }

    #[tokio::test]
    async fn lessons_go_out_on_schedule_once() {
        let (db, path) = setup().await;
        advance(&db, "2026-02-01T10:00:30Z").await.unwrap();
        assert_eq!(deliver(&db, 1).await, ["L0"], "day 0 right away");
        assert!(deliver(&db, 2).await.is_empty(), "joined before the course");
        assert!(deliver(&db, 3).await.is_empty(), "imported");

        // Day 1 isn't due until the same time next day; repeat runs are no-ops.
        advance(&db, "2026-02-02T09:59:00Z").await.unwrap();
        assert_eq!(deliver(&db, 1).await, ["L0"]);
        advance(&db, "2026-02-02T10:00:00Z").await.unwrap();
        advance(&db, "2026-02-02T10:00:00Z").await.unwrap();
        assert_eq!(deliver(&db, 1).await, ["L0", "L1"]);

        advance(&db, "2026-02-05T00:00:00Z").await.unwrap();
        assert_eq!(deliver(&db, 1).await, ["L0", "L1", "L2"]);
        advance(&db, "2026-02-05T00:01:00Z").await.unwrap();
        let (st,): (String,) =
            sqlx::query_as("SELECT status FROM sequence_enrollments WHERE subscription_id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(st, "done");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn one_lesson_in_flight_and_paused_lessons_hold() {
        let (db, path) = setup().await;
        // Overdue on everything: still only one lesson at a time, and only after it's delivered.
        advance(&db, "2026-02-09T00:00:00Z").await.unwrap();
        advance(&db, "2026-02-09T00:00:00Z").await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM campaign_sends")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(deliver(&db, 1).await, ["L0"]);
        exec(
            &db,
            "UPDATE campaigns SET status = 'paused' WHERE subject = 'L1'",
        )
        .await;
        advance(&db, "2026-02-09T00:00:00Z").await.unwrap();
        assert_eq!(
            deliver(&db, 1).await,
            ["L0"],
            "L2 must not overtake paused L1"
        );
        exec(
            &db,
            "UPDATE campaigns SET status = 'sending' WHERE subject = 'L1'",
        )
        .await;
        advance(&db, "2026-02-09T00:00:00Z").await.unwrap();
        assert_eq!(deliver(&db, 1).await, ["L0", "L1"]);
        // A definitive failure moves on.
        advance(&db, "2026-02-09T00:00:00Z").await.unwrap();
        exec(
            &db,
            "UPDATE campaign_sends SET status = 'failed' WHERE status = 'queued'",
        )
        .await;
        advance(&db, "2026-02-09T00:00:00Z").await.unwrap();
        advance(&db, "2026-02-09T00:00:00Z").await.unwrap();
        let (st,): (String,) =
            sqlx::query_as("SELECT status FROM sequence_enrollments WHERE subscription_id = 1")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(st, "done");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn resubscribing_restores_a_skipped_lesson() {
        let (db, path) = setup().await;
        advance(&db, "2026-02-01T10:00:30Z").await.unwrap();
        deliver(&db, 1).await;
        advance(&db, "2026-02-02T10:00:00Z").await.unwrap(); // L1 queued…
        exec(
            &db,
            "UPDATE subscriptions SET status = 'unsubscribed' WHERE id = 1",
        )
        .await;
        exec(
            &db,
            "UPDATE campaign_sends SET status = 'skipped' WHERE status = 'queued'",
        )
        .await; // …worker skips it
        advance(&db, "2026-02-04T00:00:00Z").await.unwrap();
        assert_eq!(deliver(&db, 1).await, ["L0"]);
        exec(
            &db,
            "UPDATE subscriptions SET status = 'confirmed' WHERE id = 1",
        )
        .await;
        advance(&db, "2026-02-04T00:00:00Z").await.unwrap();
        assert_eq!(deliver(&db, 1).await, ["L0", "L1"], "L1 restored, not lost");

        // Same again for L2, but it's moved to day 20 while they're away: restored only then.
        advance(&db, "2026-02-04T00:00:00Z").await.unwrap(); // L2 queued
        exec(
            &db,
            "UPDATE subscriptions SET status = 'unsubscribed' WHERE id = 1",
        )
        .await;
        exec(
            &db,
            "UPDATE campaign_sends SET status = 'skipped' WHERE status = 'queued'",
        )
        .await;
        exec(
            &db,
            "UPDATE campaigns SET delay_days = 20 WHERE subject = 'L2'",
        )
        .await;
        exec(
            &db,
            "UPDATE subscriptions SET status = 'confirmed' WHERE id = 1",
        )
        .await;
        advance(&db, "2026-02-05T00:00:00Z").await.unwrap();
        assert_eq!(
            deliver(&db, 1).await,
            ["L0", "L1"],
            "not before its new day"
        );
        advance(&db, "2026-02-21T10:00:00Z").await.unwrap();
        assert_eq!(deliver(&db, 1).await, ["L0", "L1", "L2"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn schedule_edits_and_added_lessons() {
        let (db, path) = setup().await;
        advance(&db, "2026-02-01T10:00:30Z").await.unwrap();
        deliver(&db, 1).await;
        // Moving the delivered day-0 lesson to day 5 neither resends it nor skips day 1.
        exec(
            &db,
            "UPDATE campaigns SET delay_days = 5 WHERE subject = 'L0'",
        )
        .await;
        // A lesson added on day 3 for day 2 has passed for this subscriber; one for day 4 hasn't.
        exec(&db, "INSERT INTO campaigns (list_id, subject, html_body, status, created_at, sequence_id, delay_days) VALUES \
             (1, 'late2', 'b', 'sending', '2026-02-04T12:00:00Z', 1, 2), (1, 'new4', 'b', 'sending', '2026-02-04T12:00:00Z', 1, 4)").await;
        for _ in 0..6 {
            advance(&db, "2026-02-07T00:00:00Z").await.unwrap();
            deliver(&db, 1).await;
        }
        let mut got = deliver(&db, 1).await;
        got.sort();
        assert_eq!(got, ["L0", "L1", "L2", "new4"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_failed_lesson_never_blocks_again() {
        let l = |id, d, written| Lesson {
            id,
            delay_days: d,
            live: true,
            written,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        // Lesson 1 failed for them, lesson 2 was sent; lesson 1 is later cleared.
        let lessons = [l(1, 0, false), l(2, 1, true), l(3, 2, true)];
        let sends = HashMap::from([(1, "failed".to_string()), (2, "sent".to_string())]);
        assert_eq!(
            next_step(
                &lessons,
                &sends,
                "2026-02-01T00:00:00Z",
                "2026-02-05T00:00:00Z"
            ),
            Next::Queue(3)
        );
    }

    #[test]
    fn nothing_overtakes_a_lesson_in_flight() {
        let l = |id, d| Lesson {
            id,
            delay_days: d,
            live: true,
            written: true,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        // B (id 2) was moved to day 0 while A (id 1, day 1) is retrying.
        let lessons = [l(2, 0), l(1, 1)];
        let sends = HashMap::from([(1, "queued".to_string())]);
        assert_eq!(
            next_step(
                &lessons,
                &sends,
                "2026-02-01T00:00:00Z",
                "2026-02-05T00:00:00Z"
            ),
            Next::Wait
        );
    }

    /// A decision made from a course that was edited meanwhile is dropped (and remade next run).
    #[tokio::test]
    async fn stale_decisions_are_dropped() {
        let (db, path) = setup().await;
        exec(&db, "UPDATE sequences SET revision = 5").await;
        let n = advance(&db, "2026-02-01T10:00:30Z").await.unwrap();
        assert_eq!(n, 1);
        // An edit between the scheduler's read (revision 4) and its write: the lock-and-check fails.
        let r =
            sqlx::query("UPDATE sequences SET revision = revision WHERE id = 1 AND revision = 4")
                .execute(&db)
                .await
                .unwrap();
        assert_eq!(r.rows_affected(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn completed_subscribers_get_lessons_added_ahead() {
        let (db, path) = setup().await;
        for t in [
            "2026-02-01T10:00:30Z",
            "2026-02-02T10:00:00Z",
            "2026-02-03T10:00:00Z",
            "2026-02-03T10:01:00Z",
        ] {
            advance(&db, t).await.unwrap();
            deliver(&db, 1).await;
        }
        // Completed; a day-7 lesson is added on day 4 and the course is reopened as add_lesson does.
        exec(&db, "INSERT INTO campaigns (list_id, subject, html_body, status, created_at, sequence_id, delay_days) \
             VALUES (1, 'L7', 'b', 'sending', '2026-02-05T00:00:00Z', 1, 7)").await;
        exec(
            &db,
            "UPDATE sequence_enrollments SET status = 'active' WHERE status = 'done'",
        )
        .await;
        advance(&db, "2026-02-08T10:00:00Z").await.unwrap();
        assert_eq!(deliver(&db, 1).await, ["L0", "L1", "L2", "L7"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn paused_course_sends_nothing() {
        let (db, path) = setup().await;
        exec(&db, "UPDATE sequences SET status = 'paused'").await;
        advance(&db, "2026-03-01T00:00:00Z").await.unwrap();
        assert!(deliver(&db, 1).await.is_empty());
        let _ = std::fs::remove_file(path);
    }
}
