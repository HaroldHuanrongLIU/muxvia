CREATE TABLE IF NOT EXISTS metadata (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS credentials (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  bearer_token TEXT NOT NULL,
  UNIQUE (target, id)
);

CREATE TABLE IF NOT EXISTS providers (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  position INTEGER NOT NULL CHECK (position >= 0),
  provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
  name TEXT NOT NULL,
  base_url TEXT NOT NULL,
  model TEXT NOT NULL,
  protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
  authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer')),
  credential_id TEXT,
  provenance_kind TEXT,
  provenance_key TEXT,
  generated_owner_id TEXT,
  routing_requirement TEXT NOT NULL DEFAULT 'direct-compatible'
    CHECK (routing_requirement IN ('direct-compatible', 'takeover-required')),
  CHECK (
    (target = 'codex' AND protocol = 'openai-responses' AND authentication = 'openai-bearer')
    OR (target = 'claude' AND protocol = 'anthropic-messages' AND authentication IN ('anthropic-api-key', 'anthropic-bearer'))
  ),
  FOREIGN KEY (target, credential_id) REFERENCES credentials(target, id)
);

CREATE TABLE IF NOT EXISTS target_route_state (
  target TEXT PRIMARY KEY CHECK (target IN ('codex', 'claude')),
  management_revision INTEGER NOT NULL,
  view_sequence INTEGER NOT NULL,
  current_provider_id TEXT,
  serving_provider_id TEXT,
  takeover_state TEXT NOT NULL,
  route_port INTEGER,
  routing_credential TEXT,
  activated_snapshot_id TEXT,
  managed_config_path TEXT,
  managed_config_version INTEGER NOT NULL DEFAULT 1 CHECK (managed_config_version IN (1,2)),
  recovery_state TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS target_problems (
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  code TEXT NOT NULL,
  message TEXT NOT NULL,
  PRIMARY KEY (target, code)
);

CREATE TABLE IF NOT EXISTS activated_snapshots (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  provider_id TEXT NOT NULL,
  base_url TEXT NOT NULL,
  model TEXT NOT NULL,
  protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
  authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer')),
  provider_bearer_token TEXT NOT NULL,
  epoch TEXT NOT NULL,
  CHECK (
    (target = 'codex' AND protocol = 'openai-responses' AND authentication = 'openai-bearer')
    OR (target = 'claude' AND protocol = 'anthropic-messages' AND authentication IN ('anthropic-api-key', 'anthropic-bearer'))
  )
);

CREATE TABLE IF NOT EXISTS action_receipts (
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  action_id TEXT NOT NULL,
  action_kind TEXT NOT NULL,
  committed_revision INTEGER NOT NULL,
  outcome_json TEXT NOT NULL,
  PRIMARY KEY (target, action_id)
);

CREATE TABLE IF NOT EXISTS activation_recovery (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  action_id TEXT NOT NULL,
  config_path TEXT NOT NULL,
  file_identity_json TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
  created_revision INTEGER NOT NULL,
  UNIQUE (target, action_id)
);

INSERT OR IGNORE INTO metadata (key, value) VALUES ('schema-version', '6');
INSERT OR IGNORE INTO target_route_state (
  target,
  management_revision,
  view_sequence,
  takeover_state,
  recovery_state
) VALUES ('codex', 0, 0, 'inactive', 'clean');
INSERT OR IGNORE INTO target_route_state (
  target,
  management_revision,
  view_sequence,
  takeover_state,
  recovery_state
) VALUES ('claude', 0, 0, 'inactive', 'clean');
