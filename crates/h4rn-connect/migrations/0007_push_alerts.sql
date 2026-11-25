-- Android push alerts (ADR-049).
--
-- One row per Android install the signed-in user registered. The FCM token is
-- a routing credential for Google, not an h4rn secret; it is nevertheless only
-- returned to the account that registered it. `device_id` is the h4rn-connect
-- device id, so sign-out and account deletion clean up the same rows.
CREATE TABLE push_devices (
    id           UUID PRIMARY KEY,
    user_id      UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    device_id    TEXT NOT NULL,
    token        TEXT NOT NULL,
    package_name TEXT,
    app_version  TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, device_id)
);
CREATE INDEX push_devices_user_idx ON push_devices (user_id);
-- A rotated token belongs to exactly one install; replacing it must not leave
-- the old token deliverable through another row.
CREATE UNIQUE INDEX push_devices_token_idx ON push_devices (token);

-- The last activity state delivered per (user, thread), so publishing the same
-- state again never alerts twice. Kept in Postgres so a relay restart does not
-- replay an alert.
CREATE TABLE push_thread_states (
    user_id    UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    thread_id  TEXT NOT NULL,
    state      TEXT NOT NULL
               CHECK (state IN ('completed', 'failed', 'approval', 'input')),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, thread_id)
);
CREATE INDEX push_thread_states_user_idx ON push_thread_states (user_id);
