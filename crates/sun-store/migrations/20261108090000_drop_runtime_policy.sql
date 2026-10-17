-- TRD BLO-117 item 14: agents own their permission mode, so `runtime_policy`
-- goes. An inheriting agent (NULL) gets the most restrictive mode among the
-- policy rows of the tools in its chain (plan < default < accept_edits <
-- bypass_permissions). Each entry resolves to its tool as placement does: an
-- object entry's credential, else its tool, names a runtime of the agent's
-- workspace, whose adapter it is; a name without a runtime row is the tool
-- itself. `market:` entries run on the seller's host and never read the
-- policy. An agent none of whose tools has a row keeps NULL: it ran on the
-- host's `sun.yaml` default and still does. One-way.
UPDATE agent SET permission_mode = (
    SELECT p.permission_mode
    FROM json_each(agent.runtime_chain) AS e
    JOIN runtime_policy AS p ON p.adapter = COALESCE(
        (SELECT r.adapter FROM runtime AS r
         WHERE r.workspace_id = agent.workspace_id
           AND r.name = CASE e.type WHEN 'object'
               THEN COALESCE(json_extract(e.value, '$.credential'), json_extract(e.value, '$.tool'))
               ELSE e.value END),
        CASE e.type WHEN 'object' THEN json_extract(e.value, '$.tool') ELSE e.value END)
    WHERE NOT (e.type = 'text' AND e.value LIKE 'market:%')
    ORDER BY CASE p.permission_mode
        WHEN 'plan' THEN 0
        WHEN 'default' THEN 1
        WHEN 'accept_edits' THEN 2
        ELSE 3 END
    LIMIT 1)
WHERE permission_mode IS NULL;

DROP TABLE runtime_policy;
