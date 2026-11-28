-- Migration 0002: the Phase 1 store (OVERVIEW §2.2, phase-1 plan §1.2).
--
-- Adds route, config_value, secret and observation; rebuilds deployment so
-- it names its revision and may lack a release (a failed build has none,
-- ADR-0007); records the phase an operation failed in (ADR-0011 rule 7);
-- and moves the state rules into triggers so the schema refuses what the
-- store API refuses.
--
-- Trigger messages start with `illegal ` for a forbidden change of state and
-- `invalid ` for a row that contradicts another; the store maps both to
-- typed errors by that prefix.

-- ---- operation ----------------------------------------------------------

-- The non-terminal phase the operation was in when it failed or was
-- interrupted. Set together with failure_code and failure_message.
ALTER TABLE operation ADD COLUMN failure_phase TEXT
    CHECK (failure_phase IS NULL OR failure_phase IN (
        'queued', 'uploading', 'validating', 'building', 'preparing',
        'deploying', 'verifying'));

CREATE INDEX operation_status ON operation (status);

-- A new operation is queued, with no failure.
CREATE TRIGGER operation_insert BEFORE INSERT ON operation
WHEN NEW.phase <> 'queued' OR NEW.status <> 'queued'
  OR NEW.failure_code IS NOT NULL OR NEW.failure_message IS NOT NULL
  OR NEW.failure_phase IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'illegal operation: a new operation is queued with no failure');
END;

CREATE TRIGGER operation_identity BEFORE UPDATE ON operation
WHEN NEW.id IS NOT OLD.id OR NEW.idempotency_key IS NOT OLD.idempotency_key
  OR NEW.kind IS NOT OLD.kind OR NEW.environment_id IS NOT OLD.environment_id
  OR NEW.created_at IS NOT OLD.created_at
BEGIN
    SELECT RAISE(ABORT, 'illegal operation change: identity fields are fixed');
END;

-- Phases move forward only, skipping any, from a non-terminal phase to a
-- later one or to a terminal one (ship_core::OperationPhase).
CREATE TRIGGER operation_phase BEFORE UPDATE OF phase ON operation
WHEN NEW.phase IS NOT OLD.phase AND NOT (
    (CASE OLD.phase
        WHEN 'queued' THEN 0 WHEN 'uploading' THEN 1 WHEN 'validating' THEN 2
        WHEN 'building' THEN 3 WHEN 'preparing' THEN 4 WHEN 'deploying' THEN 5
        WHEN 'verifying' THEN 6 END) IS NOT NULL
    AND (
        NEW.phase IN ('succeeded', 'failed', 'interrupted')
        OR (CASE NEW.phase
            WHEN 'queued' THEN 0 WHEN 'uploading' THEN 1 WHEN 'validating' THEN 2
            WHEN 'building' THEN 3 WHEN 'preparing' THEN 4 WHEN 'deploying' THEN 5
            WHEN 'verifying' THEN 6 END)
           > (CASE OLD.phase
            WHEN 'queued' THEN 0 WHEN 'uploading' THEN 1 WHEN 'validating' THEN 2
            WHEN 'building' THEN 3 WHEN 'preparing' THEN 4 WHEN 'deploying' THEN 5
            WHEN 'verifying' THEN 6 END)))
BEGIN
    SELECT RAISE(ABORT, 'illegal operation phase transition');
END;

-- Status is always the phase's status (OperationPhase::status).
CREATE TRIGGER operation_status_matches_phase BEFORE UPDATE ON operation
WHEN NEW.status IS NOT (CASE NEW.phase
    WHEN 'queued' THEN 'queued' WHEN 'succeeded' THEN 'succeeded'
    WHEN 'failed' THEN 'failed' WHEN 'interrupted' THEN 'interrupted'
    ELSE 'running' END)
BEGIN
    SELECT RAISE(ABORT, 'illegal operation status: it must match the phase');
END;

-- A terminal operation's phase, status and failure never change again.
CREATE TRIGGER operation_terminal BEFORE UPDATE ON operation
WHEN OLD.phase IN ('succeeded', 'failed', 'interrupted') AND (
    NEW.phase IS NOT OLD.phase OR NEW.status IS NOT OLD.status
    OR NEW.failure_code IS NOT OLD.failure_code
    OR NEW.failure_message IS NOT OLD.failure_message
    OR NEW.failure_phase IS NOT OLD.failure_phase)
BEGIN
    SELECT RAISE(ABORT, 'illegal operation change: the operation has ended');
END;

-- Entering `failed` or `interrupted` records the code, the message and the
-- phase it was in; every other phase has no failure.
CREATE TRIGGER operation_failure BEFORE UPDATE ON operation
WHEN OLD.phase NOT IN ('succeeded', 'failed', 'interrupted') AND (
    CASE WHEN NEW.phase IN ('failed', 'interrupted')
        THEN NEW.failure_code IS NULL OR NEW.failure_message IS NULL
             OR NEW.failure_phase IS NOT OLD.phase
        ELSE NEW.failure_code IS NOT NULL OR NEW.failure_message IS NOT NULL
             OR NEW.failure_phase IS NOT NULL
    END)
BEGIN
    SELECT RAISE(ABORT, 'illegal operation failure: set exactly on failed or interrupted');
END;

CREATE TRIGGER operation_log_cursor BEFORE UPDATE OF log_cursor ON operation
WHEN NEW.log_cursor < OLD.log_cursor
BEGIN
    SELECT RAISE(ABORT, 'illegal operation log cursor: it never moves back');
END;

-- ---- environment --------------------------------------------------------

-- running <-> stopped, either to removed; removed is final
-- (ship_core::DesiredState).
CREATE TRIGGER environment_desired_state BEFORE UPDATE OF desired_state ON environment
WHEN NEW.desired_state IS NOT OLD.desired_state AND OLD.desired_state = 'removed'
BEGIN
    SELECT RAISE(ABORT, 'illegal desired state transition');
END;

CREATE TRIGGER environment_identity BEFORE UPDATE ON environment
WHEN NEW.id IS NOT OLD.id OR NEW.project_id IS NOT OLD.project_id
  OR NEW.created_at IS NOT OLD.created_at
BEGIN
    SELECT RAISE(ABORT, 'illegal environment change: identity fields are fixed');
END;

-- ---- deployment ---------------------------------------------------------

-- Rebuilt: names its revision, and its release is set once the build
-- produced one. One deployment per operation, so a replayed operation
-- cannot create a second.
CREATE TABLE deployment_v2 (
    id                  TEXT    NOT NULL PRIMARY KEY,
    environment_id      TEXT    NOT NULL REFERENCES environment (id),
    revision_id         TEXT    NOT NULL REFERENCES revision (id),
    release_id          TEXT    REFERENCES release (id),
    operation_id        TEXT    NOT NULL UNIQUE REFERENCES operation (id),
    result              TEXT    CHECK (result IS NULL OR result IN ('succeeded', 'failed', 'interrupted')),
    previous_release_id TEXT    REFERENCES release (id),
    started_at          INTEGER NOT NULL,
    ended_at            INTEGER
) STRICT;

INSERT INTO deployment_v2
    (id, environment_id, revision_id, release_id, operation_id, result,
     previous_release_id, started_at, ended_at)
SELECT d.id, d.environment_id, r.revision_id, d.release_id, d.operation_id, d.result,
       d.previous_release_id, d.started_at, d.ended_at
  FROM deployment d JOIN release r ON r.id = d.release_id;

DROP TABLE deployment;
ALTER TABLE deployment_v2 RENAME TO deployment;

CREATE INDEX deployment_environment ON deployment (environment_id, started_at);

-- A deployment starts open, under an operation of its own environment, from
-- a revision of that environment's project, with a release (if any) built
-- from that revision.
CREATE TRIGGER deployment_insert BEFORE INSERT ON deployment
BEGIN
    SELECT RAISE(ABORT, 'illegal deployment: a new deployment has no result')
     WHERE NEW.result IS NOT NULL OR NEW.ended_at IS NOT NULL;
    -- Unknown ids are left to the foreign keys.
    SELECT RAISE(ABORT, 'invalid deployment: its operation belongs to another environment')
     WHERE EXISTS (SELECT 1 FROM operation o
                    WHERE o.id = NEW.operation_id
                      AND o.environment_id IS NOT NEW.environment_id);
    SELECT RAISE(ABORT, 'invalid deployment: its revision belongs to another project')
     WHERE EXISTS (SELECT 1 FROM revision r, environment e
                    WHERE r.id = NEW.revision_id AND e.id = NEW.environment_id
                      AND r.project_id IS NOT e.project_id);
    SELECT RAISE(ABORT, 'invalid deployment: its release was built from another revision')
     WHERE EXISTS (SELECT 1 FROM release l
                    WHERE l.id = NEW.release_id AND l.revision_id IS NOT NEW.revision_id);
END;

-- Identity fields never change; the release and the result are each set at
-- most once; a result comes with its end time; only a deployment with a
-- release can succeed.
CREATE TRIGGER deployment_update BEFORE UPDATE ON deployment
BEGIN
    SELECT RAISE(ABORT, 'illegal deployment change: identity fields are fixed')
     WHERE NEW.id IS NOT OLD.id OR NEW.environment_id IS NOT OLD.environment_id
        OR NEW.revision_id IS NOT OLD.revision_id OR NEW.operation_id IS NOT OLD.operation_id
        OR NEW.previous_release_id IS NOT OLD.previous_release_id
        OR NEW.started_at IS NOT OLD.started_at;
    SELECT RAISE(ABORT, 'illegal deployment change: the release is set once, before the end')
     WHERE NEW.release_id IS NOT OLD.release_id
       AND (OLD.release_id IS NOT NULL OR OLD.result IS NOT NULL);
    SELECT RAISE(ABORT, 'illegal deployment change: the result is set once')
     WHERE OLD.result IS NOT NULL
       AND (NEW.result IS NOT OLD.result OR NEW.ended_at IS NOT OLD.ended_at);
    SELECT RAISE(ABORT, 'illegal deployment change: a result needs an end time')
     WHERE (NEW.result IS NULL) <> (NEW.ended_at IS NULL);
    SELECT RAISE(ABORT, 'illegal deployment change: success needs a release')
     WHERE NEW.result = 'succeeded' AND NEW.release_id IS NULL;
    SELECT RAISE(ABORT, 'invalid deployment: its release was built from another revision')
     WHERE NEW.release_id IS NOT OLD.release_id
       AND EXISTS (SELECT 1 FROM release l
                    WHERE l.id = NEW.release_id AND l.revision_id IS NOT NEW.revision_id);
END;

-- Writing the release or the result a second time is refused even when the
-- value is unchanged: these fire on the columns named in SET, not on a
-- difference between OLD and NEW.
CREATE TRIGGER deployment_release_once BEFORE UPDATE OF release_id ON deployment
BEGIN
    SELECT RAISE(ABORT, 'illegal deployment change: the release is set once, before the end')
     WHERE OLD.release_id IS NOT NULL OR OLD.result IS NOT NULL;
END;

CREATE TRIGGER deployment_result_once BEFORE UPDATE OF result, ended_at ON deployment
BEGIN
    SELECT RAISE(ABORT, 'illegal deployment change: the result is set once')
     WHERE OLD.result IS NOT NULL;
END;

-- ---- audit_event --------------------------------------------------------

CREATE TRIGGER audit_event_no_update BEFORE UPDATE ON audit_event
BEGIN
    SELECT RAISE(ABORT, 'illegal audit event change: the audit log is append-only');
END;

CREATE TRIGGER audit_event_no_delete BEFORE DELETE ON audit_event
BEGIN
    SELECT RAISE(ABORT, 'illegal audit event change: the audit log is append-only');
END;

-- ---- route (ADR-0010) ---------------------------------------------------

-- Phase 1 has the `port` provider only; tailnet and caddy arrive in Phase 2.
-- Only a `confirmed` route's address is ever printed (ADR-0010 rule 2).
CREATE TABLE route (
    id             TEXT    NOT NULL PRIMARY KEY,
    environment_id TEXT    NOT NULL REFERENCES environment (id),
    service_id     TEXT    NOT NULL REFERENCES service (id),
    provider       TEXT    NOT NULL CHECK (provider IN ('port')),
    address        TEXT    NOT NULL CHECK (length(address) BETWEEN 1 AND 255),
    status         TEXT    NOT NULL CHECK (status IN ('pending', 'confirmed', 'failed')),
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
) STRICT;

CREATE INDEX route_environment ON route (environment_id);
CREATE INDEX route_service ON route (service_id);

-- ---- config_value (ADR-0009 rule 1) -------------------------------------

-- Non-secret only. Exactly the foreign-key column named by `scope` is set.
CREATE TABLE config_value (
    scope          TEXT    NOT NULL CHECK (scope IN ('project', 'environment', 'service')),
    project_id     TEXT    REFERENCES project (id),
    environment_id TEXT    REFERENCES environment (id),
    service_id     TEXT    REFERENCES service (id),
    key            TEXT    NOT NULL CHECK (length(key) BETWEEN 1 AND 256),
    value          TEXT    NOT NULL,
    updated_at     INTEGER NOT NULL,
    CHECK ((scope = 'project') = (project_id IS NOT NULL)),
    CHECK ((scope = 'environment') = (environment_id IS NOT NULL)),
    CHECK ((scope = 'service') = (service_id IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX config_value_key
    ON config_value (scope, coalesce(project_id, environment_id, service_id), key);

-- ---- secret (ADR-0009 rules 2-3) ----------------------------------------

-- Metadata only: the value lives in the secret store, found through
-- backend_ref. There is deliberately no column for a value.
CREATE TABLE secret (
    scope          TEXT    NOT NULL CHECK (scope IN ('project', 'environment', 'service')),
    project_id     TEXT    REFERENCES project (id),
    environment_id TEXT    REFERENCES environment (id),
    service_id     TEXT    REFERENCES service (id),
    name           TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 256),
    backend_ref    TEXT    NOT NULL CHECK (length(backend_ref) BETWEEN 1 AND 256),
    updated_at     INTEGER NOT NULL,
    CHECK ((scope = 'project') = (project_id IS NOT NULL)),
    CHECK ((scope = 'environment') = (environment_id IS NOT NULL)),
    CHECK ((scope = 'service') = (service_id IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX secret_name
    ON secret (scope, coalesce(project_id, environment_id, service_id), name);

-- ---- observation (ADR-0006) ---------------------------------------------

-- The latest observation of each service. Docker unreachable is stored as
-- reachable = 0 with no container state or health: nothing is known.
CREATE TABLE observation (
    service_id      TEXT    NOT NULL PRIMARY KEY REFERENCES service (id),
    reachable       INTEGER NOT NULL CHECK (reachable IN (0, 1)),
    container_state TEXT    CHECK (container_state IS NULL OR container_state IN (
        'absent', 'created', 'running', 'restarting', 'paused', 'exited', 'dead')),
    exit_code       INTEGER,
    health          TEXT    CHECK (health IS NULL OR health IN ('none', 'starting', 'passing', 'failing')),
    oom_killed      INTEGER NOT NULL DEFAULT 0 CHECK (oom_killed IN (0, 1)),
    restarts        INTEGER NOT NULL DEFAULT 0 CHECK (restarts >= 0),
    observed_at     INTEGER NOT NULL,
    CHECK ((reachable = 1) = (container_state IS NOT NULL)),
    CHECK ((reachable = 1) = (health IS NOT NULL)),
    CHECK ((exit_code IS NOT NULL) = (container_state IS 'exited'))
) STRICT;
