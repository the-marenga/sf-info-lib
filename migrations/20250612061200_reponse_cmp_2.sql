-- Add migration script here

ALTER TABLE player_info ADD COLUMN rank INT;

ALTER TABLE player ADD COLUMN class SMALLINT;

ALTER TABLE player ADD COLUMN server_player_id INT;
