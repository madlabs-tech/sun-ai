-- T31b: ids that may be used only once until they expire (a webhook
-- delivery id, a git connect state's nonce). Expired rows are pruned on
-- insert (`SettingRepo::use_once`). `expires_at` is RFC 3339 to the
-- second, so the text sorts as time.
CREATE TABLE used_once (
    key        TEXT PRIMARY KEY,
    expires_at TEXT NOT NULL
) STRICT;
CREATE INDEX used_once_expires_at ON used_once (expires_at);
