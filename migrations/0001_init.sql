-- VMDesk initial schema. IDs are UUIDv7 text, times are unix ms UTC.
CREATE TABLE providers (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    name          TEXT NOT NULL,
    config_name   TEXT NOT NULL DEFAULT '{}',
    last_sync_at  INTEGER
);

CREATE TABLE vms (
    id                TEXT PRIMARY KEY,
    provider_id       TEXT NOT NULL REFERENCES providers(id),
    external_ref      TEXT,
    name              TEXT NOT NULL,
    username          TEXT NOT NULL,
    credential_ref    TEXT NOT NULL,
    lifecycle         TEXT NOT NULL DEFAULT 'unknown',
    favourite         INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    UNIQUE (provider_id, external_ref)
);

CREATE TABLE vm_tags (
    vm_id       TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    tag         TEXT NOT NULL,
    PRIMARY KEY (vm_id, tag)
);

CREATE TABLE vm_labels (
    vm_id       TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    source      TEXT NOT NULL,
    PRIMARY KEY (vm_id, key)
);

CREATE TABLE endpoints (
    id              TEXT PRIMARY KEY,
    vm_id           TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    address         TEXT NOT NULL,
    port            INTEGER NOT NULL DEFAULT 22,
    kind            TEXT NOT NULL,
    source          TEXT NOT NULL,
    bind_interface  TEXT,
    last_success_at INTEGER,
    last_interface  TEXT
);

CREATE TABLE connection_paths (
    id              TEXT PRIMARY KEY,
    vm_id           TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    priority        INTEGER NOT NULL,
    last_success_at INTEGER
);

CREATE TABLE hops (
    path_id        TEXT NOT NULL REFERENCES connection_paths(id) ON DELETE CASCADE,
    position       INTEGER NOT NULL,
    kind           TEXT NOT NULL,
    params_json    TEXT NOT NULL,
    PRIMARY KEY (path_id, position)
);

CREATE TABLE known_hosts (
    vm_id           TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    key_type        TEXT NOT NULL,
    fingerprint     TEXT NOT NULL,
    first_seen_at   INTEGER NOT NULL,
    confirmed       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (vm_id, key_type)
);

CREATE TABLE status (
    vm_id           TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    route           TEXT,
    ssh             TEXT,
    agent           TEXT,
    check_at        INTEGER NOT NULL,
    detail          TEXT
);

CREATE TABLE agents (
    vm_id           TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    agent_id        TEXT NOT NULL,
    version         TEXT NOT NULL,
    proto           INTEGER NOT NULL,
    installed_at    INTEGER,
    last_seq        INTEGER NOT NULL DEFAULT 0,
    last_contact_at INTEGER
);

CREATE TABLE snapshot (
    id              TEXT PRIMARY KEY,
    vm_id           TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    taken_at        INTEGER NOT NULL,
    source          TEXT NOT NULL,
    schema_version  INTEGER NOT NULL,
    data_zstd       BLOB NOT NULL
);

CREATE INDEX snapshot_vm_time ON snapshot (vm_id, taken_at);

CREATE TABLE events (
    vm_id           TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    source          TEXT NOT NULL,
    seq             INTEGER NOT NULL,
    ts              INTEGER NOT NULL,
    kind            TEXT NOT NULL,
    data_json       TEXT NOT NULL,
    PRIMARY KEY     (vm_id, source, seq)
);

CREATE index events_vm_time ON events (vm_id, ts);

CREATE TABLE work_sessions (
    id             TEXT PRIMARY KEY,
    vm_id          TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER,
    title          TEXT,
    summary_json   TEXT
);

CREATE TABLE notes (
    id             TEXT PRIMARY KEY,
    vm_id          TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    session_id     TEXT REFERENCES work_sessions(id) ON DELETE SET NULL,
    body           TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE terminal_sessions (
    id             TEXT PRIMARY KEY,
    vm_id          TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER,
    path_id        TEXT REFERENCES connection_paths(id) ON DELETE SET NULL
);

INSERT INTO providers (id, kind, name) VALUES ('static', 'static', 'Manual');