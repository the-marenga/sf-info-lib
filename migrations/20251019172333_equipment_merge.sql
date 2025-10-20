-- Add migration script here
ALTER TABLE player
ADD column equipment INT ARRAY;
