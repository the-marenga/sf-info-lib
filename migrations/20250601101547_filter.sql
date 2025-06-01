-- Add migration script here
DROP INDEX equipment_lookup_idx;
ALTER TABLE EQUIPMENT ADD COLUMN attributes int not null DEFAULT 0;
UPDATE equipment e SET attributes = (SELECT attributes FROM player WHERE player.player_id = e.player_id);
CREATE INDEX equipment_lookup_idx ON equipment (server_id, attributes, ident) INCLUDE (player_id);
