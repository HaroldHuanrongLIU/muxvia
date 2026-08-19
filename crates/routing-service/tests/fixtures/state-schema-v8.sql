CREATE TABLE metadata (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE credentials (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  bearer_token TEXT NOT NULL,
  UNIQUE (target, id)
);

CREATE TABLE providers (
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

CREATE TABLE target_route_state (
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
  recovery_intent_id TEXT,
  recovery_state TEXT NOT NULL
);

CREATE TABLE target_problems (
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  code TEXT NOT NULL,
  message TEXT NOT NULL,
  source TEXT CHECK (source IS NULL OR source IN (
    'codex-profile', 'claude-managed', 'claude-shared', 'claude-project',
    'claude-local', 'claude-selector', 'claude-host-managed'
  )),
  selector TEXT CHECK (selector IS NULL OR selector IN (
    'CLAUDE_CODE_USE_BEDROCK', 'CLAUDE_CODE_USE_VERTEX',
    'CLAUDE_CODE_USE_FOUNDRY', 'CLAUDE_CODE_USE_MANTLE',
    'CLAUDE_CODE_USE_ANTHROPIC_AWS', 'CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST'
  )),
  PRIMARY KEY (target, code)
);

CREATE TABLE activated_snapshots (
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

CREATE TABLE action_receipts (
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  action_id TEXT NOT NULL,
  action_kind TEXT NOT NULL,
  committed_revision INTEGER NOT NULL,
  outcome_json TEXT NOT NULL,
  PRIMARY KEY (target, action_id)
);

CREATE TABLE activation_recovery (
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

CREATE TABLE target_compatibility (
  target TEXT PRIMARY KEY CHECK (target IN ('codex', 'claude')),
  observed_version TEXT NOT NULL,
  classification TEXT NOT NULL CHECK (classification IN ('tested', 'unknown-compatible', 'incompatible')),
  acknowledged_version TEXT,
  CHECK (acknowledged_version IS NULL OR classification = 'unknown-compatible')
);

CREATE TABLE reconciliation_intents (
  action_id TEXT NOT NULL,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  strategy TEXT NOT NULL CHECK (strategy IN ('adopt', 'reapply', 'restore')),
  state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
  created_revision INTEGER NOT NULL CHECK (created_revision >= 0),
  before_json TEXT NOT NULL,
  desired_json TEXT NOT NULL,
  PRIMARY KEY (target, action_id)
);

INSERT INTO metadata (key, value) VALUES ('schema-version', '8');
INSERT INTO target_route_state (
  target, management_revision, view_sequence, takeover_state, recovery_state
) VALUES ('codex', 0, 0, 'inactive', 'clean');
INSERT INTO target_route_state (
  target, management_revision, view_sequence, takeover_state, recovery_state
) VALUES ('claude', 0, 0, 'inactive', 'clean');
