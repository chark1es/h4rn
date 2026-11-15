-- Commit-ordered skill sync (council round 2, G3).
--
-- now() is the transaction start time, not the commit time. Two concurrent
-- pushes can start in one order and commit in the other, so a pull that saw
-- the later one advanced its cursor past the earlier row forever. A per-user
-- counter, incremented while holding that user's clock row lock, is ordered by
-- commit instead: the lock is released only when the transaction commits, so
-- the next transaction cannot receive a sequence until the earlier one is
-- durable and visible.

CREATE TABLE skill_sync_clock (
    user_id  UUID PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    last_seq BIGINT NOT NULL DEFAULT 0
);

ALTER TABLE skill_sync_entries ADD COLUMN sync_seq BIGINT;

-- Existing rows get a stable order from their receive time.
UPDATE skill_sync_entries AS entry
   SET sync_seq = ordered.seq
  FROM (
        SELECT user_id,
               skill_id,
               row_number() OVER (
                   PARTITION BY user_id ORDER BY server_updated_at, skill_id
               ) AS seq
          FROM skill_sync_entries
       ) AS ordered
 WHERE entry.user_id = ordered.user_id AND entry.skill_id = ordered.skill_id;

ALTER TABLE skill_sync_entries ALTER COLUMN sync_seq SET NOT NULL;

CREATE UNIQUE INDEX skill_sync_entries_user_seq_idx
    ON skill_sync_entries (user_id, sync_seq);

-- Seed each user's clock past the rows that were just backfilled.
INSERT INTO skill_sync_clock (user_id, last_seq)
SELECT user_id, MAX(sync_seq) FROM skill_sync_entries GROUP BY user_id
ON CONFLICT (user_id) DO UPDATE SET last_seq = EXCLUDED.last_seq;
