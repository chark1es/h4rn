-- A turn that started is an Android Live Update (ADR-053).
--
-- The delivered baseline already keys on the transition id, so this state
-- needs no new table: it only has to be an allowed value. A relay older than
-- this migration rejects the frame before any phone is told.
ALTER TABLE push_thread_states
    DROP CONSTRAINT IF EXISTS push_thread_states_state_check;

ALTER TABLE push_thread_states
    ADD CONSTRAINT push_thread_states_state_check
    CHECK (state IN ('working', 'completed', 'failed', 'approval', 'input', 'limit'));
