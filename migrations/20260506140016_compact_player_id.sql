-- Optimize PostgreSQL settings for bulk UPDATE throughput
-- Prevents hash-join memory starvation and speeds up index rebuilds
SET work_mem = '8GB';
SET maintenance_work_mem = '8GB';


-- Drop indexes that would need maintenance during the UPDATEs.
-- Without this, every row update would force 2 random btree ops per index
-- (delete old entry, insert new entry). Rebuilding from scratch is much faster.
DROP INDEX IF EXISTS player_info_idx;
DROP INDEX IF EXISTS player_stats_idx;
DROP INDEX IF EXISTS idx_player_server_active_honor;

-- Create a mapping table to store the relationship between old and new IDs
CREATE TEMP TABLE player_id_mapping AS
SELECT
    player_id AS old_id,
    row_number() OVER (ORDER BY player_id) AS new_id
FROM
    player;

-- Add a primary key on the mapping table so the UPDATEs can use an
-- Index Nested Loop Join instead of a Hash Join.
-- Without this, the hash join needs ~6-8 GB RAM for 190M entries.
-- If work_mem is insufficient, PostgreSQL batches the hash join,
-- re-scanning the entire player_info table for each batch — disastrous.
ALTER TABLE player_id_mapping ADD PRIMARY KEY (old_id);

-- Drop the foreign key constraint from player_info
-- The default name for this constraint is player_info_player_id_fkey
ALTER TABLE player_info DROP CONSTRAINT IF EXISTS player_info_player_id_fkey;

-- Update the player_id in player_info using the mapping
-- player_info_idx is already dropped, so this is pure heap writes + WAL
UPDATE player_info
SET player_id = m.new_id
FROM player_id_mapping m
WHERE player_info.player_id = m.old_id;

-- Update the player_id in player
-- To avoid primary key conflicts during the update, we first drop the primary key
-- CASCADE will also drop any FK references that might depend on it, but we'll re-add the PK
ALTER TABLE player DROP CONSTRAINT IF EXISTS player_pkey CASCADE;

-- player_stats_idx and idx_player_server_active_honor are already dropped,
-- so this UPDATE is just heap writes + WAL with no index maintenance
UPDATE player
SET player_id = m.new_id
FROM player_id_mapping m
WHERE player.player_id = m.old_id;

-- Restore the primary key on player
ALTER TABLE player ADD PRIMARY KEY (player_id);

-- Rebuild the indexes that were dropped earlier
-- These are built as sequential sort-then-write operations (fast)
CREATE INDEX player_info_idx ON player_info (player_id);

CREATE INDEX player_stats_idx ON player (level, attributes) INCLUDE (player_id, name)
    WHERE is_removed = false;

CREATE INDEX idx_player_server_active_honor ON player (server_id, honor DESC, player_id)
    INCLUDE (guild_id, name, level)
    WHERE honor IS NOT NULL AND is_removed = FALSE;

-- Restore the foreign key on player_info
ALTER TABLE player_info ADD CONSTRAINT player_info_player_id_fkey
    FOREIGN KEY (player_id) REFERENCES player(player_id);

-- Reset the sequence for player_id to the new maximum value
-- If the table is empty, we set it to 1 and is_called to false
SELECT setval('player_player_id_seq', (SELECT COALESCE(MAX(player_id), 1) FROM player), (SELECT EXISTS (SELECT 1 FROM player)));


-- Reset configuration to defaults (for safety in case the connection is reused)
RESET work_mem;
RESET maintenance_work_mem;
