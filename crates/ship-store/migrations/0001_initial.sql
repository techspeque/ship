-- Migration 0001: the Phase 0 data model (OVERVIEW §2.2).
--
-- Ids are the ship-core string form (`prj_…`, `env_…`, …). Times are Unix
-- milliseconds. Enumerated columns are checked against the ship-core string
-- forms. The Compose project name is not stored: it derives from the
-- environment id (ADR-0001 rule 4).

CREATE TABLE project (
    id         TEXT    NOT NULL PRIMARY KEY,
    slug       TEXT    NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE environment (
    id              TEXT    NOT NULL PRIMARY KEY,
    project_id      TEXT    NOT NULL REFERENCES project (id),
    name            TEXT    NOT NULL,
    desired_state   TEXT    NOT NULL
        CHECK (desired_state IN ('running', 'stopped', 'removed')),
    recovery_policy TEXT    NOT NULL DEFAULT 'observe_only'
        CHECK (recovery_policy IN ('observe_only', 'repair_safe_drift')),
    created_at      INTEGER NOT NULL,
    UNIQUE (project_id, name)
) STRICT;

CREATE TABLE service (
    id             TEXT    NOT NULL PRIMARY KEY,
    environment_id TEXT    NOT NULL REFERENCES environment (id),
    name           TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 63),
    kind           TEXT    NOT NULL CHECK (kind IN ('web', 'worker', 'job', 'compose')),
    port           INTEGER CHECK (port IS NULL OR port BETWEEN 1 AND 65535),
    -- Health check configuration as JSON; NULL when none is configured.
    health_config  TEXT,
    created_at     INTEGER NOT NULL,
    UNIQUE (environment_id, name)
) STRICT;

-- Immutable (ADR-0007 rule 1).
CREATE TABLE revision (
    id                   TEXT    NOT NULL PRIMARY KEY,
    project_id           TEXT    NOT NULL REFERENCES project (id),
    kind                 TEXT    NOT NULL CHECK (kind IN ('snapshot', 'git', 'image')),
    -- Content hash, resolved commit or image digest, per kind.
    source_ref           TEXT    NOT NULL,
    manifest_fingerprint TEXT,
    created_at           INTEGER NOT NULL
) STRICT;

CREATE INDEX revision_project ON revision (project_id);

CREATE TRIGGER revision_immutable BEFORE UPDATE ON revision
BEGIN
    SELECT RAISE(ABORT, 'revision rows are immutable');
END;

-- Immutable (ADR-0007 rule 1). Never holds a secret value (rule 3).
CREATE TABLE release (
    id                 TEXT    NOT NULL PRIMARY KEY,
    revision_id        TEXT    NOT NULL REFERENCES revision (id),
    -- JSON array of image ids or digests.
    images             TEXT    NOT NULL,
    config_fingerprint TEXT    NOT NULL,
    created_at         INTEGER NOT NULL
) STRICT;

CREATE INDEX release_revision ON release (revision_id);

CREATE TRIGGER release_immutable BEFORE UPDATE ON release
BEGIN
    SELECT RAISE(ABORT, 'release rows are immutable');
END;

-- ADR-0011.
CREATE TABLE operation (
    id              TEXT    NOT NULL PRIMARY KEY,
    idempotency_key TEXT    NOT NULL UNIQUE CHECK (length(idempotency_key) > 0),
    kind            TEXT    NOT NULL CHECK (length(kind) > 0),
    environment_id  TEXT    REFERENCES environment (id),
    phase           TEXT    NOT NULL CHECK (phase IN (
        'queued', 'uploading', 'validating', 'building', 'preparing',
        'deploying', 'verifying', 'succeeded', 'failed', 'interrupted')),
    status          TEXT    NOT NULL CHECK (status IN (
        'queued', 'running', 'succeeded', 'failed', 'interrupted')),
    failure_code    TEXT,
    failure_message TEXT,
    log_cursor      INTEGER NOT NULL DEFAULT 0 CHECK (log_cursor >= 0),
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX operation_environment ON operation (environment_id);

-- Sequence numbers are per operation, start at 1 and have no gaps.
CREATE TABLE operation_event (
    operation_id TEXT    NOT NULL REFERENCES operation (id),
    sequence     INTEGER NOT NULL CHECK (sequence >= 1),
    type         TEXT    NOT NULL,
    phase        TEXT    NOT NULL CHECK (phase IN (
        'queued', 'uploading', 'validating', 'building', 'preparing',
        'deploying', 'verifying', 'succeeded', 'failed', 'interrupted')),
    message      TEXT    NOT NULL,
    ts           INTEGER NOT NULL,
    PRIMARY KEY (operation_id, sequence)
) STRICT, WITHOUT ROWID;

CREATE TABLE deployment (
    id                  TEXT    NOT NULL PRIMARY KEY,
    environment_id      TEXT    NOT NULL REFERENCES environment (id),
    release_id          TEXT    NOT NULL REFERENCES release (id),
    operation_id        TEXT    NOT NULL REFERENCES operation (id),
    result              TEXT    CHECK (result IS NULL OR result IN ('succeeded', 'failed', 'interrupted')),
    previous_release_id TEXT    REFERENCES release (id),
    started_at          INTEGER NOT NULL,
    ended_at            INTEGER
) STRICT;

CREATE INDEX deployment_environment ON deployment (environment_id, started_at);

CREATE TABLE audit_event (
    id     INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    actor  TEXT    NOT NULL,
    action TEXT    NOT NULL,
    target TEXT    NOT NULL,
    ts     INTEGER NOT NULL
) STRICT;
