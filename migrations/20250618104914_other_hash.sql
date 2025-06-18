-- Add migration script here
ALTER TABLE otherplayer_resp DROP CONSTRAINT otherplayer_resp_hash_key;

DROP INDEX otherplayer_resp_hash_key;

CREATE INDEX otherplayer_resp_hash_key on otherplayer_resp using HASH(hash);
