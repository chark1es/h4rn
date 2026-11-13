-- Skill sync (Phase 4, ADR-034, ADR-035).
--
-- h4rn-connect is a mailbox, not a skills authority: the daemon registry is
-- canonical and every row here is a last-writer-wins snapshot plus a conflict
-- marker. Bundle bytes are envelope-encrypted before they reach this table, so
-- a database dump does not contain skill contents. Metadata (name, version,
-- vector) is plaintext so the server can compare causality without decrypting.

CREATE TABLE skill_sync_entries (
    user_id           UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    skill_id          TEXT NOT NULL,
    name              TEXT NOT NULL,
    description       TEXT NOT NULL,
    version           TEXT NOT NULL,
    -- Serialised SkillVersionVector. TEXT rather than JSONB so the relay does
    -- not need sqlx's JSON feature; it is never queried by shape.
    vector_json       TEXT NOT NULL,
    etag              TEXT NOT NULL,
    deleted           BOOLEAN NOT NULL DEFAULT false,
    -- XChaCha20-Poly1305 envelope-encrypted bundle payload.
    dek_nonce         BYTEA NOT NULL,
    wrapped_dek       BYTEA NOT NULL,
    nonce             BYTEA NOT NULL,
    ciphertext        BYTEA NOT NULL,
    -- Sender clock, used only as the last-writer-wins tie breaker.
    updated_at        TIMESTAMPTZ NOT NULL,
    -- Server clock, monotonic per row and used as the pull cursor.
    server_updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by_device UUID REFERENCES devices (id) ON DELETE SET NULL,
    PRIMARY KEY (user_id, skill_id)
);
CREATE INDEX skill_sync_entries_user_updated_idx
    ON skill_sync_entries (user_id, server_updated_at, skill_id);

CREATE TABLE skill_sync_conflicts (
    id            BIGSERIAL PRIMARY KEY,
    user_id       UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    skill_id      TEXT NOT NULL,
    local_version TEXT,
    remote_version TEXT NOT NULL,
    reason        TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at   TIMESTAMPTZ
);
CREATE INDEX skill_sync_conflicts_user_idx
    ON skill_sync_conflicts (user_id, created_at DESC);
