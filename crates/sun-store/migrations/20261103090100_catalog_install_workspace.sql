-- M5: catalog installs (skills and MCP servers) are per workspace. The
-- installed files stay shared on disk (`<data>/skills/<id>`,
-- `<data>/mcp_servers/<id>.json`, their versions in
-- `<data>/catalog/installed.json`); a row here says a workspace has one, and
-- a run gets only its workspace's skills and catalog MCP servers. The next
-- migration gives Main every install from before.
CREATE TABLE workspace_catalog_install (
    workspace_id TEXT NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
    catalog_id   TEXT NOT NULL,
    installed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now')),
    PRIMARY KEY (workspace_id, catalog_id)
) STRICT;
CREATE INDEX workspace_catalog_install_by_entry ON workspace_catalog_install (catalog_id);
