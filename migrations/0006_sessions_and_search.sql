-- Work sessions and notes are read per VM; notes also move with their session.
CREATE INDEX work_sessions_vm ON work_sessions (vm_id, started_at);
CREATE INDEX notes_vm ON notes (vm_id, created_at);
CREATE INDEX notes_session ON notes (session_id);
-- Re-deriving sessions reads every gap of a VM, however old.
CREATE INDEX events_gaps ON events (vm_id, ts) WHERE kind = 'gap';

-- Event search (spec section 13): the searchable text of each event, indexed with FTS5.
-- The index stores no copy of the text; it is read from this view by rowid. A VACUUM
-- can renumber `events` rowids, so run `INSERT INTO events_fts(events_fts)
-- VALUES ('rebuild')` after one.
CREATE VIEW events_text AS
SELECT rowid AS id, concat_ws(' ',
    json_extract(data_json, '$.cmdline'),
    json_extract(data_json, '$.cwd'),
    json_extract(data_json, '$.pane_cwd'),
    json_extract(data_json, '$.repo'),
    json_extract(data_json, '$.branch'),
    CASE WHEN kind = 'git_branch_changed' THEN json_extract(data_json, '$.from') END,
    CASE WHEN kind = 'git_branch_changed' THEN json_extract(data_json, '$.to') END,
    json_extract(data_json, '$.subject'),
    json_extract(data_json, '$.name'),
    json_extract(data_json, '$.image'),
    json_extract(data_json, '$.compose_project'),
    json_extract(data_json, '$.process'),
    json_extract(data_json, '$.port')
) AS text
FROM events WHERE kind NOT IN ('status_changed', 'gap');

CREATE VIRTUAL TABLE events_fts USING fts5 (text, content = 'events_text', content_rowid = 'id');

CREATE TRIGGER events_fts_insert AFTER INSERT ON events BEGIN
    INSERT INTO events_fts (rowid, text) SELECT id, text FROM events_text WHERE id = new.rowid;
END;

-- Before the delete: the index needs the old text, which the view reads from the row.
CREATE TRIGGER events_fts_delete BEFORE DELETE ON events BEGIN
    INSERT INTO events_fts (events_fts, rowid, text)
    SELECT 'delete', id, text FROM events_text WHERE id = old.rowid;
END;

INSERT INTO events_fts (events_fts) VALUES ('rebuild');
