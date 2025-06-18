-- Add migration script here
ALTER TABLE otherplayer_resp
DROP CONSTRAINT otherplayer_resp_hash_key;

ALTER TABLE otherplayer_resp
DROP COLUMN hash;

CREATE INDEX otherplayer_resp_hash on otherplayer_resp using HASH (otherplayer_resp);
