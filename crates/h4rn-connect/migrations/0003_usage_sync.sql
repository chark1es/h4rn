-- Phase 5: durable, idempotent usage sync and cross-machine reporting (ADR-024).
--
-- A daemon retries batches after a network failure, so `row_id` is the
-- account-scoped idempotency key. Older rows predate the column and stay NULL.

ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS row_id TEXT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS workspace TEXT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS machine TEXT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS account TEXT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS tokens_cached_creation BIGINT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS cost_source TEXT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS price_table TEXT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS generation_ms BIGINT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS wall_ms BIGINT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS started_at TIMESTAMPTZ;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS completed_at TIMESTAMPTZ;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS correlation_id TEXT;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS deduped BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS synced_at TIMESTAMPTZ;

CREATE UNIQUE INDEX IF NOT EXISTS usage_events_user_row_idx
    ON usage_events (user_id, row_id)
    WHERE row_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS usage_events_user_started_idx
    ON usage_events (user_id, started_at DESC);
CREATE INDEX IF NOT EXISTS usage_events_user_workspace_idx
    ON usage_events (user_id, workspace, ts DESC);
