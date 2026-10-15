-- path ownership, unique VM names, ssh-config provider.
ALTER TABLE connection_paths ADD COLUMN source TEXT NOT NULL DEFAULT 'user';

CREATE UNIQUE INDEX vms_name_unique ON vms (name COLLATE NOCASE);

INSERT INTO providers (id, kind, name) VALUES ('ssh-config', 'ssh-config', 'SSH config import');