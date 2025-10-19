-- Add migration script here
ALTER TABLE player
ADD column equipment INT ARRAY;

UPDATE player p
SET equipment = e.idents
FROM (
    SELECT player_id, array_agg(ident) AS idents
    FROM equipment
    GROUP BY player_id
) e
WHERE p.player_id = e.player_id;
