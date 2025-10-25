DROP INDEX IF EXISTS player_info_idx;

ALTER TABLE player_info
     DROP COLUMN xp,
     ALTER COLUMN level TYPE int2 USING level::int2,
     ALTER COLUMN soldier_advice TYPE int2 USING soldier_advice::int2;

CREATE INDEX player_info_idx ON player_info (player_id);
