-- Add a column that holds the original Protocol-v1 framed bytes
-- (`0xBF <varint length> <protobuf TakMessage>`) for each persisted
-- event. Nullable so historic rows persist; new inserts always
-- populate it.
--
-- Why a separate column instead of round-tripping the decomposed
-- (uid, type, lat, lon, hae, detail, ...) columns:
--
-- 1. Re-encoding from the decomposed view is lossy on any TakMessage
--    extension field that wasn't decomposed (status proto, mission
--    annotations, federation hints — none are columns today).
-- 2. Replay-on-reconnect (Tier-1 punch-list item #2 in the drop-in
--    readiness assessment) needs byte-perfect bytes to satisfy the
--    `pli_dispatch_byte_identity` invariant on replayed events.
-- 3. Disk cost is bounded — average CoT frame is ~200-1000 bytes
--    and rows already retain the decomposed columns. Doubles row
--    size in the worst case, which is dwarfed by the GiST geo
--    index.

ALTER TABLE public.cot_router
    ADD COLUMN wire_bytes BYTEA NULL;

-- Index the servertime column we use for "events in the last N
-- seconds" queries on subscribe. cot_router already has indices
-- from upstream Java migrations, but explicitly naming the one we
-- depend on protects against reordering or removal.
CREATE INDEX IF NOT EXISTS cot_router_servertime_replay_idx
    ON public.cot_router (servertime DESC)
    WHERE wire_bytes IS NOT NULL;
