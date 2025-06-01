-- Add migration script here
CREATE INDEX equipment_lookup_idx ON equipment (server_id, attributes, ident)
INCLUDE (player_id);
