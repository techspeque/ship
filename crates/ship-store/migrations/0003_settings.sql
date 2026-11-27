-- Migration 0003: daemon settings (phase-1 plan, decisions).
--
-- Settings the operator sets on the host, never from a pushed directory:
-- `publish.address` (host scope) now, `compose.confirm` (host or project
-- scope) with phase-1-ws-1.8. Exactly the project column is set for a
-- project-scoped setting.

CREATE TABLE setting (
    scope      TEXT    NOT NULL CHECK (scope IN ('host', 'project')),
    project_id TEXT    REFERENCES project (id),
    key        TEXT    NOT NULL CHECK (length(key) BETWEEN 1 AND 64),
    value      TEXT    NOT NULL CHECK (length(value) BETWEEN 1 AND 256),
    updated_at INTEGER NOT NULL,
    CHECK ((scope = 'project') = (project_id IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX setting_key ON setting (scope, coalesce(project_id, ''), key);
