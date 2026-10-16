-- TRD BLO-117 item 5: a limit hit cools the account that hit it, not every
-- runtime of that name. `account` is `cred:<runtime id>` for a `byo_key` or
-- `credits` runtime, `login:<host id>:<tool>` for a CLI's own login
-- (`sun_core::model::Account`). `runtime_state` stays for history.
CREATE TABLE account_cooldown (
    account TEXT PRIMARY KEY,
    until   TEXT NOT NULL
) STRICT;

-- Cooldowns still running carry over: a key's as is, and a CLI login's on
-- every host of the runtime's workspace (another workspace's host never
-- cools on it).
INSERT INTO account_cooldown (account, until)
SELECT account, MAX(until) FROM (
    SELECT 'cred:' || r.id AS account, s.cooldown_until AS until
    FROM runtime_state s JOIN runtime r ON r.id = s.runtime_id
    WHERE s.status = 'cooling_down' AND s.cooldown_until IS NOT NULL
      AND r.source != 'byo_runtime'
    UNION ALL
    SELECT 'login:' || h.host_id || ':' || r.adapter, s.cooldown_until
    FROM runtime_state s
    JOIN runtime r ON r.id = s.runtime_id
    JOIN workspace_host h ON h.workspace_id = r.workspace_id
    WHERE s.status = 'cooling_down' AND s.cooldown_until IS NOT NULL
      AND r.source = 'byo_runtime'
)
GROUP BY account;
