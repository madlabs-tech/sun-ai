-- Per-runtime permission mode set from the Runtimes page (1.16e), sent with
-- every session of that adapter. No row: the host's `sun.yaml` default.
CREATE TABLE runtime_policy (
    adapter         TEXT PRIMARY KEY,
    permission_mode TEXT NOT NULL
) STRICT;
