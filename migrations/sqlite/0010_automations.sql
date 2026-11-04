-- Automations: a trigger plus a tree of steps (send email, wait, if/else, tag, list, goal, exit),
-- edited as a vertical flowchart. Replaces email courses: existing courses are converted at
-- startup (automation::convert_courses), after which the sequences tables are no longer used.
CREATE TABLE automations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  list_id INTEGER NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  -- draft | active | paused
  status TEXT NOT NULL DEFAULT 'draft',
  -- JSON: what enrolls a subscriber, e.g. {"type":"subscribed"} or {"type":"tag_added","tag_id":3}.
  trigger_config TEXT NOT NULL DEFAULT '{"type":"subscribed"}',
  -- JSON array of steps; an if/else step holds its own "yes" and "no" step arrays.
  steps TEXT NOT NULL DEFAULT '[]',
  created_at TEXT NOT NULL,
  -- First activation. Triggers only count events from then on.
  started_at TEXT,
  -- Bumped by every edit; the scheduler's writes require it unchanged.
  revision INTEGER NOT NULL DEFAULT 0,
  -- The email course this was converted from, if any.
  legacy_sequence_id INTEGER UNIQUE
);
CREATE INDEX idx_automations_list ON automations(list_id);

-- Email steps are campaign rows with automation_id set (status stays 'sending', like course lessons).
ALTER TABLE campaigns ADD COLUMN automation_id INTEGER REFERENCES automations(id) ON DELETE CASCADE;
CREATE INDEX idx_campaigns_automation ON campaigns(automation_id);

-- One row per subscriber going through an automation (each person goes through it once).
CREATE TABLE automation_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  automation_id INTEGER NOT NULL REFERENCES automations(id) ON DELETE CASCADE,
  subscription_id INTEGER NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
  -- active | done
  status TEXT NOT NULL DEFAULT 'active',
  -- The step they're at (not yet completed); '' once done.
  step_id TEXT NOT NULL DEFAULT '',
  -- When they reached that step (waits count from here).
  entered_at TEXT NOT NULL,
  created_at TEXT NOT NULL,
  finished_at TEXT,
  UNIQUE (automation_id, subscription_id)
);
CREATE INDEX idx_automation_runs_status ON automation_runs(automation_id, status);

-- Tags belong to the person (site-wide), not to one newsletter subscription.
CREATE TABLE tags (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  site_id INTEGER NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE (site_id, name)
);
CREATE TABLE subscriber_tags (
  subscriber_id INTEGER NOT NULL REFERENCES subscribers(id) ON DELETE CASCADE,
  tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  added_at TEXT NOT NULL,
  PRIMARY KEY (subscriber_id, tag_id)
);
CREATE INDEX idx_subscriber_tags_tag ON subscriber_tags(tag_id, added_at);
-- Every time someone gets a tag (kept after the tag is removed again), for "gets a tag" triggers.
CREATE TABLE tag_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  subscriber_id INTEGER NOT NULL REFERENCES subscribers(id) ON DELETE CASCADE,
  tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  added_at TEXT NOT NULL
);
CREATE INDEX idx_tag_events_tag ON tag_events(tag_id, added_at);

-- Tracked links: each distinct URL in a campaign gets a random key; the email links to
-- /c/{key}/{token}. Keys are unguessable so link destinations can't be enumerated,
-- and the redirect only ever goes to the stored URL. Links outlive their campaign (campaign_id
-- becomes NULL) so links in emails already sent keep working.
CREATE TABLE links (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  key TEXT NOT NULL UNIQUE,
  campaign_id INTEGER REFERENCES campaigns(id) ON DELETE SET NULL,
  url TEXT NOT NULL,
  UNIQUE (campaign_id, url)
);

-- Opens (tracking pixel; approximate, since some mail apps load images automatically) and clicks.
CREATE TABLE engagements (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  campaign_id INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
  subscription_id INTEGER NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
  -- open | click
  kind TEXT NOT NULL,
  link_id INTEGER REFERENCES links(id) ON DELETE SET NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_engagements_sub ON engagements(subscription_id, campaign_id, kind);
CREATE INDEX idx_engagements_campaign ON engagements(campaign_id, kind, created_at);
