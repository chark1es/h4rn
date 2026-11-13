-- Machine connection candidates (ADR-009, Phase 3).
--
-- A machine can be reachable over the LAN, a tailnet address or the relay.
-- Clients read this list from `/v1/machines` and probe the candidates in
-- order; the daemon replaces the whole list on each report because a laptop
-- changes networks. The relay URL is stored as a `relay:`-prefixed string,
-- matching the daemon's advertisement format.

CREATE TABLE machine_addresses (
    id         UUID PRIMARY KEY,
    machine_id UUID NOT NULL REFERENCES machines (id) ON DELETE CASCADE,
    address    TEXT NOT NULL,
    ordinal    INTEGER NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (machine_id, address)
);
CREATE INDEX machine_addresses_machine_idx ON machine_addresses (machine_id, ordinal);
