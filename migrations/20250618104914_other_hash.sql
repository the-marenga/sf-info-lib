-- Add migration script here
DROP INDEX otherplayer_resp_hash_key;

CREATE INDEX otherplayer_resp_hash_key on otherplayer_resp using HASH(hash);
