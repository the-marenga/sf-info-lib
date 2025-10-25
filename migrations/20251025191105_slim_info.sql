drop INDEX player_info_idx;
CREATE INDEX player_info_idx ON player_info (player_id);

ALTER TABLE player_info drop column xp, drop column soldier_advice;

ALTER TABLE player_info
ALTER COLUMN level TYPE int2
USING level::int2;
