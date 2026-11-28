-- Migration 0005: lifecycle commands (phase-1-ws-1.12).
--
-- A desired state per service, beside the environment's: `ship stop P/S`
-- records one service as stopped, and its status derives from it. The
-- environment's desired state wins when it is `stopped` or `removed`
-- (ship_core::DesiredState::effective). A service is never `removed` on
-- its own: a release and a Compose project cover the whole environment.
--
-- A deployment has an action. `deploy` applies a release (every row from
-- before this migration). `remove` records the removal of the environment
-- in the same history: it names the revision that was serving, records no
-- release and succeeds without one. A deploy records the options it was
-- asked with (`--file`, `--port`, as JSON), so `ship redeploy` can repeat
-- it; NULL for a removal and for a row from before.

ALTER TABLE service ADD COLUMN desired_state TEXT NOT NULL DEFAULT 'running'
    CHECK (desired_state IN ('running', 'stopped'));

ALTER TABLE deployment ADD COLUMN action TEXT NOT NULL DEFAULT 'deploy'
    CHECK (action IN ('deploy', 'remove'));

ALTER TABLE deployment ADD COLUMN options TEXT;

-- ---- deployment ---------------------------------------------------------

-- As in 0002, and a removal carries no release.
DROP TRIGGER deployment_insert;
CREATE TRIGGER deployment_insert BEFORE INSERT ON deployment
BEGIN
    SELECT RAISE(ABORT, 'illegal deployment: a new deployment has no result')
     WHERE NEW.result IS NOT NULL OR NEW.ended_at IS NOT NULL;
    SELECT RAISE(ABORT, 'illegal deployment: a removal records no release or options')
     WHERE NEW.action = 'remove' AND (NEW.release_id IS NOT NULL OR NEW.options IS NOT NULL);
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

-- As in 0002, with the action and the options fixed, and only a deploy
-- needing a release to succeed.
DROP TRIGGER deployment_update;
CREATE TRIGGER deployment_update BEFORE UPDATE ON deployment
BEGIN
    SELECT RAISE(ABORT, 'illegal deployment change: identity fields are fixed')
     WHERE NEW.id IS NOT OLD.id OR NEW.environment_id IS NOT OLD.environment_id
        OR NEW.revision_id IS NOT OLD.revision_id OR NEW.operation_id IS NOT OLD.operation_id
        OR NEW.previous_release_id IS NOT OLD.previous_release_id
        OR NEW.started_at IS NOT OLD.started_at
        OR NEW.action IS NOT OLD.action OR NEW.options IS NOT OLD.options;
    SELECT RAISE(ABORT, 'illegal deployment change: the release is set once, before the end')
     WHERE NEW.release_id IS NOT OLD.release_id
       AND (OLD.release_id IS NOT NULL OR OLD.result IS NOT NULL);
    SELECT RAISE(ABORT, 'illegal deployment change: a removal records no release')
     WHERE NEW.action = 'remove' AND NEW.release_id IS NOT NULL;
    SELECT RAISE(ABORT, 'illegal deployment change: the result is set once')
     WHERE OLD.result IS NOT NULL
       AND (NEW.result IS NOT OLD.result OR NEW.ended_at IS NOT OLD.ended_at);
    SELECT RAISE(ABORT, 'illegal deployment change: a result needs an end time')
     WHERE (NEW.result IS NULL) <> (NEW.ended_at IS NULL);
    SELECT RAISE(ABORT, 'illegal deployment change: success needs a release')
     WHERE NEW.result = 'succeeded' AND NEW.release_id IS NULL AND NEW.action = 'deploy';
    SELECT RAISE(ABORT, 'invalid deployment: its release was built from another revision')
     WHERE NEW.release_id IS NOT OLD.release_id
       AND EXISTS (SELECT 1 FROM release l
                    WHERE l.id = NEW.release_id AND l.revision_id IS NOT NEW.revision_id);
END;

-- ---- environment --------------------------------------------------------

-- running <-> stopped, either to removed (ship_core::DesiredState). Out of
-- removed only to running, and only once a deploy of the environment
-- succeeded after its latest removal: `ship push --recreate` (Human,
-- 2026-11-28). An environment removed before it had any deployment has no
-- removal row; any later deploy counts.
DROP TRIGGER environment_desired_state;
CREATE TRIGGER environment_desired_state BEFORE UPDATE OF desired_state ON environment
WHEN NEW.desired_state IS NOT OLD.desired_state AND OLD.desired_state = 'removed'
 AND NOT (NEW.desired_state = 'running' AND EXISTS (
     SELECT 1 FROM deployment d
      WHERE d.environment_id = NEW.id AND d.action = 'deploy' AND d.result = 'succeeded'
        AND d.started_at >= COALESCE(
            (SELECT MAX(r.ended_at) FROM deployment r
              WHERE r.environment_id = NEW.id AND r.action = 'remove'), 0)))
BEGIN
    SELECT RAISE(ABORT, 'illegal desired state transition');
END;
