-- Connected cloud accounts used for automatic setup (secrets inside `config` are encrypted).
CREATE TABLE integrations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,              -- 'aws' | 'porkbun'
  name TEXT NOT NULL,
  config TEXT NOT NULL,
  -- e.g. the AWS account id / caller ARN found when the credentials were checked.
  details TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL
);
CREATE INDEX idx_integrations_account ON integrations(account_id);

-- Progress and created resources for a site's automatic SES + DNS setup.
CREATE TABLE site_setups (
  site_id INTEGER PRIMARY KEY REFERENCES sites(id) ON DELETE CASCADE,
  status TEXT NOT NULL,            -- 'running' | 'applied' | 'error'
  state TEXT NOT NULL,             -- JSON: steps, created resources, errors
  updated_at TEXT NOT NULL
);
