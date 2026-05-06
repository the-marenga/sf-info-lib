-- Create a mapping table to store the relationship between old and new IDs
CREATE TEMP TABLE player_id_mapping AS
SELECT
    player_id AS old_id,
    row_number() OVER (ORDER BY player_id) AS new_id
FROM
    player;

-- Drop the foreign key constraint from player_info
-- The default name for this constraint is player_info_player_id_fkey
ALTER TABLE player_info DROP CONSTRAINT IF EXISTS player_info_player_id_fkey;

-- Update the player_id in player_info using the mapping
UPDATE player_info
SET player_id = m.new_id
FROM player_id_mapping m
WHERE player_info.player_id = m.old_id;

-- Update the player_id in player
-- To avoid primary key conflicts during the update, we first drop the primary key
-- CASCADE will also drop any indices that might depend on it, but we'll re-add the PK
ALTER TABLE player DROP CONSTRAINT IF EXISTS player_pkey CASCADE;

UPDATE player
SET player_id = m.new_id
FROM player_id_mapping m
WHERE player.player_id = m.old_id;

-- Restore the primary key on player
ALTER TABLE player ADD PRIMARY KEY (player_id);

-- Restore the foreign key on player_info
ALTER TABLE player_info ADD CONSTRAINT player_info_player_id_fkey
    FOREIGN KEY (player_id) REFERENCES player(player_id);

-- Reset the sequence for player_id to the new maximum value
-- If the table is empty, we set it to 1 and is_called to false
SELECT setval('player_player_id_seq', (SELECT COALESCE(MAX(player_id), 1) FROM player), (SELECT EXISTS (SELECT 1 FROM player)));
