-- Add migration script here
AlTER TABLE player ADD column guild_id INT REFERENCES guild(guild_id);

WITH bad_guilds AS (
    SELECT guild_id FROM guild where name = ''
)
UPDATE player_info
SET guild_id = NULL WHERE guild_id in (SELECT guild_id FROM bad_guilds);

UPDATE player
SET guild_id = (
    SELECT pi.guild_id
    FROM player_info pi
    WHERE pi.player_id = player.player_id
    ORDER BY pi.fetch_time DESC
    LIMIT 1
);

CREATE INDEX idx_player_server_active_honor ON player (server_id, honor DESC, player_id)
INCLUDE (guild_id, name, level)
WHERE honor IS NOT NULL AND is_removed = FALSE;

CREATE UNIQUE INDEX player_name_lookup ON player (server_id, lower(name));
