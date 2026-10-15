-- Align column and table names with the spec (section 13).
ALTER TABLE providers RENAME COLUMN config_name TO config_json;
ALTER TABLE status RENAME COLUMN check_at TO checked_at;

DROP INDEX snapshot_vm_time;
ALTER TABLE snapshot RENAME TO snapshots;
CREATE INDEX snapshots_vm_time ON snapshots (vm_id, taken_at);
