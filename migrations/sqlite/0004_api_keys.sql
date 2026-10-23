-- API keys for scripts and AI agents (`Authorization: Bearer lc_…`). Only a SHA-256 hash is stored.
CREATE TABLE api_keys (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  prefix TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL,
  last_used_at TEXT,
  expires_at TEXT
);
CREATE INDEX idx_api_keys_user ON api_keys(user_id);
