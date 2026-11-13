-- h4rn-connect schema (ADR-012).
--
-- Postgres is the system of record for accounts, devices, pairings, presence
-- and the usage ledger. The daemon's SQLite journal is only a cache. Migrations
-- are applied by `sqlx::migrate!` at boot and are additive: a running relay can
-- roll forward without a dump/restore.

CREATE TABLE users (
    id          UUID PRIMARY KEY,
    email       TEXT,
    display_name TEXT,
    avatar_url  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per (issuer, subject). OrbitID subjects are public and stable.
CREATE TABLE identities (
    id             UUID PRIMARY KEY,
    user_id        UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    provider_id    TEXT NOT NULL,
    issuer         TEXT NOT NULL,
    subject        TEXT NOT NULL,
    email          TEXT,
    email_verified BOOLEAN NOT NULL DEFAULT false,
    claims         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (issuer, subject)
);
CREATE INDEX identities_user_idx ON identities (user_id);
CREATE INDEX identities_provider_idx ON identities (provider_id);

CREATE TABLE machines (
    id               UUID PRIMARY KEY,
    user_id          UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name             TEXT NOT NULL,
    -- The daemon device that owns this machine. Set after the daemon registers.
    daemon_device_id UUID,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX machines_user_idx ON machines (user_id);

CREATE TABLE devices (
    id                     UUID PRIMARY KEY,
    user_id                UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    machine_id             UUID REFERENCES machines (id) ON DELETE SET NULL,
    kind                   TEXT NOT NULL CHECK (kind IN ('daemon', 'desktop', 'mobile', 'web')),
    name                   TEXT NOT NULL,
    -- Ed25519 public key used for DPoP-style proofs, raw 32 bytes.
    proof_public_key       BYTEA NOT NULL,
    -- X25519 Noise static public key, raw 32 bytes.
    noise_static_public_key BYTEA,
    platform               TEXT,
    last_seen              TIMESTAMPTZ,
    revoked_at             TIMESTAMPTZ,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX devices_proof_key_idx ON devices (proof_public_key);
CREATE INDEX devices_user_idx ON devices (user_id);
CREATE INDEX devices_machine_idx ON devices (machine_id);

ALTER TABLE machines
    ADD CONSTRAINT machines_daemon_device_fk
    FOREIGN KEY (daemon_device_id) REFERENCES devices (id) ON DELETE SET NULL;

-- Addressable hosts on a machine, including WSL distributions (ADR-019).
CREATE TABLE machine_hosts (
    id         UUID PRIMARY KEY,
    machine_id UUID NOT NULL REFERENCES machines (id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('host', 'wsl')),
    distro     TEXT,
    last_seen  TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (machine_id, name)
);

-- Which client devices may open a relay session to which machine. Created by
-- claiming a pairing; the daemon learns the same set over the relay.
CREATE TABLE machine_devices (
    machine_id UUID NOT NULL REFERENCES machines (id) ON DELETE CASCADE,
    device_id  UUID NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    paired_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (machine_id, device_id)
);
CREATE INDEX machine_devices_device_idx ON machine_devices (device_id);

-- Refresh-token family. One row per refresh token; rotation inserts a new row
-- in the same family and marks the old one rotated (ADR-011).
CREATE TABLE sessions (
    id               UUID PRIMARY KEY,
    family_id        UUID NOT NULL,
    user_id          UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    device_id        UUID NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    refresh_hash     TEXT NOT NULL UNIQUE,
    access_jti       UUID NOT NULL,
    issued_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at       TIMESTAMPTZ NOT NULL,
    rotated_at       TIMESTAMPTZ,
    replaced_by      UUID REFERENCES sessions (id) ON DELETE SET NULL,
    -- The successor refresh token, envelope-encrypted for the idempotent retry
    -- window only. Nulled/ignored after the grace period.
    successor_secret BYTEA,
    revoked_at       TIMESTAMPTZ,
    revoke_reason    TEXT,
    user_agent       TEXT,
    ip               TEXT
);
CREATE INDEX sessions_family_idx ON sessions (family_id);
CREATE INDEX sessions_device_idx ON sessions (device_id);
CREATE INDEX sessions_user_idx ON sessions (user_id);

-- Server-side OIDC login flows: state + PKCE verifier + nonce. The browser
-- never sees the verifier.
CREATE TABLE auth_flows (
    id                      UUID PRIMARY KEY,
    provider_id             TEXT NOT NULL,
    state                   TEXT NOT NULL UNIQUE,
    nonce                   TEXT NOT NULL,
    pkce_verifier           TEXT NOT NULL,
    redirect_uri            TEXT NOT NULL,
    device_kind             TEXT NOT NULL,
    device_name             TEXT NOT NULL,
    proof_public_key        BYTEA NOT NULL,
    noise_static_public_key BYTEA,
    platform                TEXT,
    machine_name            TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at              TIMESTAMPTZ NOT NULL,
    consumed_at             TIMESTAMPTZ
);
CREATE INDEX auth_flows_expiry_idx ON auth_flows (expires_at);

-- Device authorization grants. The daemon polls connect; connect polls the
-- provider with the stored device code.
CREATE TABLE device_authorizations (
    id                      UUID PRIMARY KEY,
    provider_id             TEXT NOT NULL,
    device_code             TEXT NOT NULL,
    user_code               TEXT NOT NULL,
    verification_uri        TEXT NOT NULL,
    verification_uri_complete TEXT,
    interval_secs           INTEGER NOT NULL DEFAULT 5,
    expires_at              TIMESTAMPTZ NOT NULL,
    status                  TEXT NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'approved', 'denied', 'expired')),
    device_kind             TEXT NOT NULL,
    device_name             TEXT NOT NULL,
    proof_public_key        BYTEA NOT NULL,
    noise_static_public_key BYTEA,
    platform                TEXT,
    machine_name            TEXT,
    user_id                 UUID REFERENCES users (id) ON DELETE SET NULL,
    device_id               UUID REFERENCES devices (id) ON DELETE SET NULL,
    -- Issued session and envelope-encrypted refresh token, so a repeated poll
    -- returns the same grant instead of minting a second session.
    session_id              UUID REFERENCES sessions (id) ON DELETE SET NULL,
    refresh_ciphertext      BYTEA,
    last_polled_at          TIMESTAMPTZ,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX device_authorizations_expiry_idx ON device_authorizations (expires_at);

CREATE TABLE pairings (
    id                  UUID PRIMARY KEY,
    machine_id          UUID NOT NULL REFERENCES machines (id) ON DELETE CASCADE,
    created_by_device_id UUID NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    secret_hash         TEXT NOT NULL,
    state               TEXT NOT NULL DEFAULT 'pending'
                        CHECK (state IN ('pending', 'claimed', 'expired', 'revoked')),
    expires_at          TIMESTAMPTZ NOT NULL,
    claimed_by_device_id UUID REFERENCES devices (id) ON DELETE SET NULL,
    claimed_at          TIMESTAMPTZ,
    attempts            INTEGER NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX pairings_machine_idx ON pairings (machine_id);
CREATE INDEX pairings_expiry_idx ON pairings (expires_at);

CREATE TABLE presence (
    device_id    UUID PRIMARY KEY REFERENCES devices (id) ON DELETE CASCADE,
    user_id      UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    machine_id   UUID REFERENCES machines (id) ON DELETE SET NULL,
    status       TEXT NOT NULL CHECK (status IN ('online', 'offline')),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX presence_machine_idx ON presence (machine_id);
CREATE INDEX presence_user_idx ON presence (user_id);

-- Envelope-encrypted secrets (ADR-016). Each row has its own DEK wrapped by
-- the KEK; `aad` binds a row to its purpose.
CREATE TABLE encrypted_secrets (
    id           UUID PRIMARY KEY,
    user_id      UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    purpose      TEXT NOT NULL,
    dek_nonce    BYTEA NOT NULL,
    wrapped_dek  BYTEA NOT NULL,
    nonce        BYTEA NOT NULL,
    ciphertext   BYTEA NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, purpose, name)
);
CREATE INDEX encrypted_secrets_user_idx ON encrypted_secrets (user_id);

-- Metadata-only audit trail. Never contains prompts, payloads or secrets.
CREATE TABLE audit_log (
    id              BIGSERIAL PRIMARY KEY,
    user_id         UUID REFERENCES users (id) ON DELETE SET NULL,
    actor_device_id UUID REFERENCES devices (id) ON DELETE SET NULL,
    action          TEXT NOT NULL,
    target_type     TEXT,
    target_id       TEXT,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    at              TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX audit_log_user_at_idx ON audit_log (user_id, id DESC);
CREATE INDEX audit_log_at_idx ON audit_log (at DESC);

-- Phase 5 usage ledger. Fixed now so metering can sync later without a
-- breaking migration (ADR-024). One row per harness turn/proxy call.
CREATE TABLE usage_events (
    id             BIGSERIAL PRIMARY KEY,
    user_id        UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    machine_id     UUID REFERENCES machines (id) ON DELETE SET NULL,
    thread_id      TEXT,
    run_id         TEXT,
    harness        TEXT,
    model          TEXT,
    provider       TEXT,
    credential_id  TEXT,
    tokens_in      BIGINT,
    tokens_out     BIGINT,
    tokens_cached  BIGINT,
    cost_micros    BIGINT,
    ttft_ms        BIGINT,
    tps            DOUBLE PRECISION,
    ts             TIMESTAMPTZ NOT NULL,
    raw            JSONB,
    source         TEXT NOT NULL DEFAULT 'self',
    reconciled     BOOLEAN NOT NULL DEFAULT false,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX usage_events_user_ts_idx ON usage_events (user_id, ts DESC);
CREATE INDEX usage_events_thread_idx ON usage_events (thread_id);
CREATE INDEX usage_events_machine_idx ON usage_events (machine_id);
