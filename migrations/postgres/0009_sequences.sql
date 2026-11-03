-- Email courses / drip sequences. Each lesson is a campaign row with sequence_id set (status stays
-- 'sending' so the normal sender delivers its queued rows; it's paused only after repeated failures).
CREATE TABLE sequences (
  id BIGSERIAL PRIMARY KEY,
  list_id BIGINT NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  -- draft | active | paused
  status TEXT NOT NULL DEFAULT 'draft',
  created_at TEXT NOT NULL,
  -- First activation. Only subscribers confirmed from then on are enrolled.
  started_at TEXT,
  -- Bumped by every lesson add/edit/delete/resume; the scheduler's writes require it unchanged.
  revision BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX idx_sequences_list ON sequences(list_id);

ALTER TABLE campaigns ADD COLUMN sequence_id BIGINT REFERENCES sequences(id) ON DELETE CASCADE;
-- Days after the subscriber confirmed (0 = right away).
ALTER TABLE campaigns ADD COLUMN delay_days BIGINT NOT NULL DEFAULT 0;

-- One row per subscriber taking a course. Progress is derived from their campaign_sends rows.
CREATE TABLE sequence_enrollments (
  sequence_id BIGINT NOT NULL REFERENCES sequences(id) ON DELETE CASCADE,
  subscription_id BIGINT NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
  enrolled_at TEXT NOT NULL,
  -- active | done
  status TEXT NOT NULL DEFAULT 'active',
  PRIMARY KEY (sequence_id, subscription_id)
);
CREATE INDEX idx_sequence_enrollments_status ON sequence_enrollments(status);
