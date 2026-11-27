-- Migration 0004: Compose confirmations (phase-1-ws-1.8).
--
-- An audit event may carry a detail: JSON naming what was confirmed or
-- accepted (the Compose file's fingerprint and its items), never a secret
-- value. A setting records who set it, so a decision taken under it can
-- name that.

ALTER TABLE audit_event ADD COLUMN detail TEXT;

ALTER TABLE setting ADD COLUMN updated_by TEXT NOT NULL DEFAULT 'operator';
