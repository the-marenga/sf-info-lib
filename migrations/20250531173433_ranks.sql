-- Add migration script here
AlTER TABLE player ADD column guild_id INT REFERENCES guild(guild_id);

WITH bad_guilds AS (
    SELECT guild_id FROM guild where name = ''
)

CREATE INDEX idx_player_server_active_honor ON player (server_id, honor DESC, player_id)
INCLUDE (guild_id, name, level)
WHERE honor IS NOT NULL AND is_removed = FALSE;
