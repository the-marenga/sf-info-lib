-- Add migration script here
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
