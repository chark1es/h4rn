-- Pairing scopes and the typed short code (ADR-029).
--
-- A pairing grants a set of scopes; claiming it copies them next to the
-- device-machine link so they can be signed into access tokens and relayed to
-- the daemon. Links that predate this migration keep every scope: they were
-- unrestricted when they were made.
ALTER TABLE pairings
    ADD COLUMN scopes TEXT[] NOT NULL DEFAULT '{view,approve,terminal,files}',
    ADD COLUMN code_hash TEXT;

-- A short code identifies at most one live pairing. Claimed, revoked and
-- expired rows are exempt, so a code can be reissued after its pairing dies.
CREATE UNIQUE INDEX pairings_code_pending_idx ON pairings (code_hash)
    WHERE state = 'pending' AND code_hash IS NOT NULL;

ALTER TABLE machine_devices
    ADD COLUMN scopes TEXT[] NOT NULL DEFAULT '{view,approve,terminal,files,admin}';
