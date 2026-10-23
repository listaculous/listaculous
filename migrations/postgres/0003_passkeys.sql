-- Stable random user handle for WebAuthn (never the numeric id or email).
ALTER TABLE users ADD COLUMN webauthn_id TEXT;
CREATE UNIQUE INDEX idx_users_webauthn_id ON users(webauthn_id);

CREATE TABLE passkeys (
  id BIGSERIAL PRIMARY KEY,
  user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  credential_id TEXT NOT NULL UNIQUE,
  -- Serialized webauthn-rs Passkey (public key, counter, flags).
  passkey TEXT NOT NULL,
  created_at TEXT NOT NULL,
  last_used_at TEXT
);

-- Short-lived, single-use ceremony state between the start and finish calls.
CREATE TABLE webauthn_challenges (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  user_id BIGINT,
  state TEXT NOT NULL,
  expires_at TEXT NOT NULL
);
