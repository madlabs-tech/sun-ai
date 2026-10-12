-- T22: the workspace profile. `workspace` is referenced by `workflow` and the
-- link tables, so its columns are added, not rebuilt. Values are validated in
-- `sun-core` (`Workspace::clean_*`); here the slug is unique and immutable.
--
-- Every existing workspace gets a slug from its name, by the rule of
-- `Workspace::slug_from_name` (ASCII letters and digits lowercased, every
-- other run one `-`, at most 48 characters, `workspace` when nothing is
-- left), de-duplicated in id order with `-2`, `-3`, ... (Main is `main`), and
-- an issue prefix of the slug's first four letters or digits, uppercased
-- (Main is `MAIN`).

ALTER TABLE workspace ADD COLUMN slug TEXT NOT NULL DEFAULT '';
ALTER TABLE workspace ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE workspace ADD COLUMN context TEXT NOT NULL DEFAULT '';
-- A blob-store hash; NULL: no logo.
ALTER TABLE workspace ADD COLUMN logo_blob TEXT;
ALTER TABLE workspace ADD COLUMN issue_prefix TEXT NOT NULL DEFAULT '';
ALTER TABLE workspace ADD COLUMN issue_counter INTEGER NOT NULL DEFAULT 0;
-- One of this workspace's workflows (checked in the engine).
ALTER TABLE workspace ADD COLUMN default_workflow_id TEXT
    REFERENCES workflow (id) ON DELETE SET NULL;

WITH RECURSIVE
    -- The name, one character at a time.
    walk (id, rest, out) AS (
        SELECT id, lower(name), '' FROM workspace
        UNION ALL
        SELECT id, substr(rest, 2),
            out || CASE
                WHEN substr(rest, 1, 1) GLOB '[a-z0-9]' THEN substr(rest, 1, 1)
                WHEN out = '' OR substr(out, -1) = '-' THEN ''
                ELSE '-'
            END
        FROM walk WHERE rest <> ''
    ),
    base (rn, id, slug) AS (
        SELECT row_number() OVER (ORDER BY id), id,
            coalesce(nullif(rtrim(substr(out, 1, 48), '-'), ''), 'workspace')
        FROM walk WHERE rest = ''
    ),
    n (k) AS (SELECT 1 UNION ALL SELECT k + 1 FROM n WHERE k <= (SELECT count(*) FROM base)),
    -- A workspace's slugs to try, in order: enough that one is always free.
    candidate (rn, k, slug) AS (
        SELECT base.rn, n.k, CASE
            WHEN n.k = 1 THEN base.slug
            ELSE rtrim(substr(base.slug, 1, 48 - length('-' || n.k)), '-') || '-' || n.k
        END
        FROM base, n
    ),
    -- One workspace per step, in id order: `taken` holds the slugs given
    -- before it, and it gets its first candidate not among them.
    given (rn, id, slug, taken) AS (
        SELECT 0, NULL, NULL, json('[]')
        UNION ALL
        SELECT base.rn, base.id,
            (SELECT c.slug FROM candidate c
             WHERE c.rn = base.rn AND NOT EXISTS (
                 SELECT 1 FROM json_each(json_insert(given.taken, '$[#]', given.slug)) t
                 WHERE t.value = c.slug)
             ORDER BY c.k LIMIT 1),
            json_insert(given.taken, '$[#]', given.slug)
        FROM given JOIN base ON base.rn = given.rn + 1
    )
UPDATE workspace SET slug = (SELECT slug FROM given WHERE given.id = workspace.id);

UPDATE workspace SET issue_prefix = upper(substr(replace(slug, '-', ''), 1, 4));

CREATE UNIQUE INDEX workspace_by_slug ON workspace (slug);

-- A slug never changes once given (links and URLs name it).
CREATE TRIGGER workspace_slug_immutable BEFORE UPDATE OF slug ON workspace
WHEN OLD.slug <> '' AND NEW.slug IS NOT OLD.slug
BEGIN
    SELECT RAISE(ABORT, 'workspace.slug is immutable');
END;
