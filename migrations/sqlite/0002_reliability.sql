ALTER TABLE campaign_sends ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
-- When a queued row may next be attempted (retry backoff), or when it was claimed while 'sending'.
ALTER TABLE campaign_sends ADD COLUMN next_attempt_at TEXT;
-- Singleton row claimed by first-run setup so two concurrent setups can't both succeed.
CREATE TABLE setup_lock (id INTEGER PRIMARY KEY);
ALTER TABLE subscriptions ADD COLUMN confirm_sent_at TEXT;
