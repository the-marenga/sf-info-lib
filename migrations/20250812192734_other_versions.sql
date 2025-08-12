-- Add migration script here
ALTER TABLE otherplayer_resp
ADD column version smallint default 1;
