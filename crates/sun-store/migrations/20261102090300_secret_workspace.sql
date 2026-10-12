-- M4: secrets a person sets belong to a workspace. Every ecosystem secret
-- but the deployment's own (`Secret::is_deployment`: the SMTP password, SSO
-- client secrets, the git host apps, the embedding key) moves to Main's
-- workspace scope: BYO keys follow their runtimes there, and MCP server
-- secrets are read from Main until MCP servers are per workspace. A name
-- Main already holds stays where it is. The sealed value names its scope
-- (AAD), so the store's data step moves and re-seals each one
-- (`sun_store::secret::move_ecosystem_secrets_to_main`), and moves the
-- names an import left to re-enter the same way; nothing is left for SQL.
SELECT 1;
