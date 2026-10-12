-- A `byo_key` runtime's source (2.06): `{endpoint, model, key_secret}` as
-- JSON. `key_secret` names a workspace secret; the key value stays in
-- `secret`. NULL for every other source.
ALTER TABLE runtime ADD COLUMN config_json TEXT;
