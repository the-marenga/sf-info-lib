drop INDEX player_info_idx;
CREATE INDEX player_info_idx ON player_info (player_id);

ALTER TABLE player_info drop column xp;

ALTER TABLE player_info
ALTER COLUMN level TYPE int2
USING level::int2;

ALTER TABLE player_info
ALTER COLUMN soldier_advice TYPE int2
USING soldier_advice::int2;
