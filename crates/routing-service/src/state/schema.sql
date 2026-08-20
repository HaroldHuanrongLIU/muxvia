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
  generated_source_revision INTEGER CHECK (generated_source_revision IS NULL OR generated_source_revision >= 1),
  generated_overlay_revision INTEGER CHECK (generated_overlay_revision IS NULL OR generated_overlay_revision >= 1),
  CHECK (
    (target = 'codex' AND protocol = 'openai-responses' AND authentication = 'openai-bearer')
    OR (target = 'claude' AND protocol = 'anthropic-messages' AND authentication IN ('anthropic-api-key', 'anthropic-bearer'))
  ),
  FOREIGN KEY (target, credential_id) REFERENCES credentials(target, id)
);

CREATE UNIQUE INDEX IF NOT EXISTS providers_generated_owner_target
  ON providers(generated_owner_id, target)
  WHERE generated_owner_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS universal_provider_catalog_state (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  revision INTEGER NOT NULL CHECK (revision >= 0),
  view_sequence INTEGER NOT NULL CHECK (view_sequence >= 0)
);

CREATE TABLE IF NOT EXISTS universal_credentials (
  id TEXT PRIMARY KEY,
  bearer_token TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS universal_providers (
  id TEXT PRIMARY KEY,
  position INTEGER NOT NULL UNIQUE CHECK (position >= 0),
  provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
  name TEXT NOT NULL,
  base_url TEXT NOT NULL,
  credential_id TEXT REFERENCES universal_credentials(id),
  provenance_kind TEXT,
  provenance_key TEXT,
  CHECK (
    (provenance_kind IS NULL AND provenance_key IS NULL)
    OR (provenance_kind IS NOT NULL AND provenance_key IS NOT NULL)
  )
);

CREATE TABLE IF NOT EXISTS universal_provider_targets (
  universal_provider_id TEXT NOT NULL REFERENCES universal_providers(id) ON DELETE CASCADE,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
  model TEXT NOT NULL,
  authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer')),
  routing_requirement TEXT NOT NULL CHECK (routing_requirement IN ('direct-compatible', 'takeover-required')),
  overlay_revision INTEGER NOT NULL CHECK (overlay_revision >= 1),
  synchronized_source_revision INTEGER CHECK (synchronized_source_revision IS NULL OR synchronized_source_revision >= 1),
  synchronized_overlay_revision INTEGER CHECK (synchronized_overlay_revision IS NULL OR synchronized_overlay_revision >= 1),
  CHECK (
    (target = 'codex' AND authentication = 'openai-bearer')
    OR (target = 'claude' AND authentication IN ('anthropic-api-key', 'anthropic-bearer'))
  ),
  CHECK (
    (synchronized_source_revision IS NULL AND synchronized_overlay_revision IS NULL)
    OR (synchronized_source_revision IS NOT NULL AND synchronized_overlay_revision IS NOT NULL)
  ),
  PRIMARY KEY (universal_provider_id, target)
);

CREATE TABLE IF NOT EXISTS universal_action_receipts (
  action_id TEXT PRIMARY KEY,
  action_kind TEXT NOT NULL,
  committed_revision INTEGER NOT NULL CHECK (committed_revision >= 0),
  outcome_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS universal_provider_seeds (
  preset_key TEXT PRIMARY KEY,
  seeded_provider_id TEXT
);

CREATE TABLE IF NOT EXISTS subscription_account_catalog_state (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  revision INTEGER NOT NULL CHECK (revision >= 0),
  view_sequence INTEGER NOT NULL CHECK (view_sequence >= 0),
  recovery_state TEXT NOT NULL CHECK (recovery_state IN ('clean', 'recovery-required'))
);

CREATE TABLE IF NOT EXISTS subscription_provider_bindings (
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  binding_kind TEXT NOT NULL CHECK (binding_kind IN ('fixed', 'follow-default')),
  account_id TEXT,
  CHECK (
    (binding_kind = 'fixed' AND account_id IS NOT NULL AND length(account_id) > 0)
    OR (binding_kind = 'follow-default' AND account_id IS NULL)
  ),
  PRIMARY KEY (target, provider_id)
);

CREATE TABLE IF NOT EXISTS subscription_account_recovery_intents (
  id TEXT PRIMARY KEY,
  action_id TEXT NOT NULL UNIQUE,
  operation TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
  before_sha256 TEXT NOT NULL,
  desired_sha256 TEXT NOT NULL,
  created_revision INTEGER NOT NULL CHECK (created_revision >= 0)
);

CREATE TABLE IF NOT EXISTS subscription_account_action_receipts (
  action_id TEXT PRIMARY KEY,
  action_kind TEXT NOT NULL,
  action_json TEXT NOT NULL,
  committed_revision INTEGER NOT NULL CHECK (committed_revision >= 0),
  outcome_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS failover_drafts (
  target TEXT PRIMARY KEY CHECK (target IN ('codex', 'claude')),
  draft_revision INTEGER NOT NULL CHECK (draft_revision >= 1)
);

CREATE TABLE IF NOT EXISTS failover_draft_members (
  target TEXT NOT NULL REFERENCES failover_drafts(target) ON DELETE CASCADE,
  position INTEGER NOT NULL CHECK (position >= 0),
  provider_id TEXT NOT NULL,
  provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
  PRIMARY KEY (target, position),
  UNIQUE (target, provider_id)
);

CREATE TABLE IF NOT EXISTS activated_route_plans (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  epoch TEXT NOT NULL,
  created_revision INTEGER NOT NULL CHECK (created_revision >= 0)
);

CREATE TABLE IF NOT EXISTS activated_route_plan_members (
  plan_id TEXT NOT NULL REFERENCES activated_route_plans(id) ON DELETE CASCADE,
  position INTEGER NOT NULL CHECK (position >= 0),
  provider_id TEXT NOT NULL,
  provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
  name TEXT NOT NULL,
  base_url TEXT NOT NULL,
  model TEXT NOT NULL,
  protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
  authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer')),
  credential_id TEXT NOT NULL REFERENCES credentials(id),
  routing_requirement TEXT NOT NULL CHECK (routing_requirement IN ('direct-compatible', 'takeover-required')),
  PRIMARY KEY (plan_id, position),
  UNIQUE (plan_id, provider_id)
);

CREATE TABLE IF NOT EXISTS provider_route_health (
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  provider_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('healthy', 'degraded', 'unavailable')),
  service_epoch TEXT NOT NULL,
  consecutive_successes INTEGER NOT NULL CHECK (consecutive_successes >= 0),
  consecutive_failures INTEGER NOT NULL CHECK (consecutive_failures >= 0),
  total_attempts INTEGER NOT NULL CHECK (total_attempts >= 0),
  failed_attempts INTEGER NOT NULL CHECK (failed_attempts >= 0 AND failed_attempts <= total_attempts),
  observation_sequence INTEGER NOT NULL CHECK (observation_sequence >= 0),
  last_outcome TEXT NOT NULL,
  PRIMARY KEY (target, provider_id)
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
  recovery_intent_id TEXT,
  active_route_plan_id TEXT REFERENCES activated_route_plans(id),
  recovery_state TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS target_problems (
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

CREATE TABLE IF NOT EXISTS target_compatibility (
  target TEXT PRIMARY KEY CHECK (target IN ('codex', 'claude')),
  observed_version TEXT NOT NULL,
  classification TEXT NOT NULL CHECK (classification IN ('tested', 'unknown-compatible', 'incompatible')),
  acknowledged_version TEXT,
  CHECK (acknowledged_version IS NULL OR classification = 'unknown-compatible')
);

CREATE TABLE IF NOT EXISTS reconciliation_intents (
  action_id TEXT NOT NULL,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  strategy TEXT NOT NULL CHECK (strategy IN ('adopt', 'reapply', 'restore')),
  state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
  created_revision INTEGER NOT NULL CHECK (created_revision >= 0),
  before_json TEXT NOT NULL,
  desired_json TEXT NOT NULL,
  PRIMARY KEY (target, action_id)
);

INSERT OR IGNORE INTO subscription_account_catalog_state
  (singleton, revision, view_sequence, recovery_state)
VALUES (1, 0, 0, 'clean');

INSERT OR IGNORE INTO metadata (key, value) VALUES ('schema-version', '11');
INSERT OR IGNORE INTO universal_provider_catalog_state (
  singleton, revision, view_sequence
) VALUES (1, 0, 0);
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
INSERT OR IGNORE INTO failover_drafts (target, draft_revision) VALUES ('codex', 1);
INSERT OR IGNORE INTO failover_drafts (target, draft_revision) VALUES ('claude', 1);
