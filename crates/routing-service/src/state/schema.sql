CREATE TABLE IF NOT EXISTS metadata (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS providers (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  name TEXT NOT NULL,
  base_url TEXT NOT NULL,
  model TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS provider_credentials (
  provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
  bearer_token TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS target_route_state (
  target TEXT PRIMARY KEY CHECK (target = 'codex'),
  management_revision INTEGER NOT NULL,
  view_sequence INTEGER NOT NULL,
  current_provider_id TEXT,
  serving_provider_id TEXT,
  takeover_state TEXT NOT NULL,
  route_port INTEGER,
  routing_credential TEXT,
  activated_snapshot_id TEXT,
  recovery_state TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS action_receipts (
  action_id TEXT PRIMARY KEY,
  action_kind TEXT NOT NULL,
  committed_revision INTEGER NOT NULL,
  outcome_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS activation_recovery (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  action_id TEXT NOT NULL UNIQUE,
  config_path TEXT NOT NULL,
  file_identity_json TEXT NOT NULL,
  before_owned_json TEXT NOT NULL,
  desired_owned_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
  created_revision INTEGER NOT NULL
);

INSERT OR IGNORE INTO metadata (key, value) VALUES ('schema-version', '1');
INSERT OR IGNORE INTO target_route_state (
  target,
  management_revision,
  view_sequence,
  takeover_state,
  recovery_state
) VALUES ('codex', 0, 0, 'inactive', 'clean');
