-- CC Switch SQLite 导出
-- 生成时间: 2026-08-21 00:00:00
-- user_version: 16
PRAGMA foreign_keys=OFF;
PRAGMA user_version=16;
BEGIN TRANSACTION;
CREATE TABLE providers (
  id TEXT NOT NULL,
  app_type TEXT NOT NULL,
  name TEXT NOT NULL,
  settings_config TEXT NOT NULL,
  website_url TEXT,
  category TEXT,
  created_at INTEGER,
  sort_index INTEGER,
  notes TEXT,
  icon TEXT,
  icon_color TEXT,
  meta TEXT NOT NULL DEFAULT '{}',
  is_current BOOLEAN NOT NULL DEFAULT 0,
  in_failover_queue BOOLEAN NOT NULL DEFAULT 0,
  PRIMARY KEY (id, app_type)
);
CREATE TABLE proxy_request_logs (
  request_id TEXT PRIMARY KEY,
  provider_id TEXT NOT NULL,
  app_type TEXT NOT NULL,
  model TEXT NOT NULL,
  request_model TEXT,
  pricing_model TEXT,
  input_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
  input_token_semantics INTEGER NOT NULL DEFAULT 0,
  input_cost_usd TEXT NOT NULL DEFAULT '0',
  output_cost_usd TEXT NOT NULL DEFAULT '0',
  cache_read_cost_usd TEXT NOT NULL DEFAULT '0',
  cache_creation_cost_usd TEXT NOT NULL DEFAULT '0',
  total_cost_usd TEXT NOT NULL DEFAULT '0',
  latency_ms INTEGER NOT NULL,
  first_token_ms INTEGER,
  duration_ms INTEGER,
  status_code INTEGER NOT NULL,
  error_message TEXT,
  session_id TEXT,
  provider_type TEXT,
  is_streaming INTEGER NOT NULL DEFAULT 0,
  cost_multiplier TEXT NOT NULL DEFAULT '1.0',
  created_at INTEGER NOT NULL,
  data_source TEXT NOT NULL DEFAULT 'proxy'
);
CREATE TABLE usage_daily_rollups (
  date TEXT NOT NULL,
  app_type TEXT NOT NULL,
  provider_id TEXT NOT NULL,
  model TEXT NOT NULL,
  request_model TEXT NOT NULL DEFAULT '',
  pricing_model TEXT NOT NULL DEFAULT '',
  request_count INTEGER NOT NULL DEFAULT 0,
  success_count INTEGER NOT NULL DEFAULT 0,
  input_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
  input_token_semantics INTEGER NOT NULL DEFAULT 0,
  total_cost_usd TEXT NOT NULL DEFAULT '0',
  avg_latency_ms INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (date, app_type, provider_id, model, request_model, pricing_model)
);
INSERT INTO providers VALUES (
  'cc-codex-1', 'codex', 'Same Name',
  '{"auth":{"OPENAI_API_KEY":"ccswitch-codex-credential-fixture"},"config":"model_provider = \"custom\"\nmodel = \"gpt-5.6-sol\"\n[model_providers.custom]\nbase_url = \"https://codex-export.example/v1\""}',
  NULL, 'custom', 1, 0, NULL, NULL, NULL, '{}', 1, 0
);
INSERT INTO providers VALUES (
  'cc-claude-1', 'claude', 'Same Name',
  '{"env":{"ANTHROPIC_BASE_URL":"https://claude-export.example","ANTHROPIC_MODEL":"claude-sonnet-4-6","ANTHROPIC_AUTH_TOKEN":"ccswitch-claude-credential-fixture"}}',
  NULL, 'custom', 2, 0, NULL, NULL, NULL, '{}', 1, 0
);
INSERT INTO proxy_request_logs VALUES (
  'codex-request-secret-identity', 'cc-codex-1', 'codex', 'gpt-5.6-sol', 'gpt-5.6-sol',
  'gpt-5.6-sol', 100, 20, 5, 2, 0, '0', '0', '0', '0', '9.99', 150, 40, 150,
  200, NULL, 'codex-session-secret-identity', NULL, 0, '1.0', 1767225600, 'proxy'
);
INSERT INTO proxy_request_logs VALUES (
  'codex-failed-secret-identity', 'cc-codex-1', 'codex', 'gpt-5.6-sol', 'gpt-5.6-sol',
  'gpt-5.6-sol', 10, 0, 0, 0, 0, '0', '0', '0', '0', '1.00', 50, NULL, 50,
  500, 'upstream-error-secret-payload', 'codex-session-secret-identity-2', NULL, 0, '1.0',
  1767225660, 'proxy'
);
INSERT INTO proxy_request_logs VALUES (
  'claude-request-secret-identity', 'cc-claude-1', 'claude', 'claude-sonnet-4-6', '',
  'claude-sonnet-4-6', 200, 40, 20, 8, 0, '0', '0', '0', '0', '4.00', 220, 60, 220,
  200, NULL, 'claude-session-secret-identity', NULL, 1, '1.0', 1767225720, 'proxy'
);
INSERT INTO usage_daily_rollups VALUES (
  '2025-12-31', 'codex', 'cc-codex-1', 'gpt-5.6-sol', 'gpt-5.6-sol', 'gpt-5.6-sol',
  3, 3, 300, 60, 30, 12, 0, '15.00', 100
);
COMMIT;
PRAGMA foreign_keys=ON;
