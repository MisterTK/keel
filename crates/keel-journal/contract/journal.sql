-- keel — journal schema, contracts-v1.
-- SQLite, WAL mode. Default location: .keel/journal.db (one per project).
-- The journal is deliberately a universally inspectable file: `keel trace`
-- is a query, and users can open it with any SQLite tool.
-- Design constraints for future backends (architecture spec §6): append-heavy,
-- idempotent writes, no cross-flow transactions except leases.
--
-- All timestamps are integer milliseconds since the Unix epoch.
-- Payloads are MessagePack with a schema tag (versioned, self-describing).

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE keel_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

INSERT INTO keel_meta (key, value) VALUES ('schema_version', '1');

-- One row per flow execution. Identity: (entrypoint, args_hash, explicit key).
CREATE TABLE flows (
    flow_id       TEXT PRIMARY KEY,             -- ULID
    entrypoint    TEXT NOT NULL,                -- e.g. 'py:pipeline.ingest:main'
    args_hash     TEXT NOT NULL,
    code_hash     TEXT,                         -- fences deploys during replay
    status        TEXT NOT NULL
                  CHECK (status IN ('running','completed','failed','dead')),
    lease_holder  TEXT,                         -- process id, NULL when unleased
    lease_expires INTEGER,                      -- ms epoch, NULL when unleased
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

-- Recovery scan: incomplete flows with expired leases.
CREATE INDEX flows_incomplete ON flows (status, lease_expires);

-- One row per journaled step. Step identity within a flow: seq (execution
-- order); replay matches on (seq, step_key) and any divergence is
-- nondeterminism (KEEL-E031, handled per flows.on_nondeterminism policy).
CREATE TABLE steps (
    flow_id     TEXT    NOT NULL REFERENCES flows (flow_id),
    seq         INTEGER NOT NULL,
    step_key    TEXT    NOT NULL,               -- '(target)#(args_hash)'
    kind        TEXT    NOT NULL
                CHECK (kind IN ('effect','time','random','subprocess','marker')),
    attempt     INTEGER NOT NULL DEFAULT 1,     -- attempts consumed by this step
    outcome     TEXT    NOT NULL
                CHECK (outcome IN ('ok','error','running')),
    payload     BLOB,                           -- MessagePack, schema-tagged
    error_class TEXT,                           -- ErrorClass when outcome='error'
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,                        -- NULL while running
    PRIMARY KEY (flow_id, seq)
);

CREATE INDEX steps_by_key ON steps (flow_id, step_key);

-- Persistent response cache (cache = { scope = "persistent" }).
CREATE TABLE cache (
    key        TEXT PRIMARY KEY,                -- '(target)#(args_hash)'
    value      BLOB NOT NULL,                   -- MessagePack, schema-tagged
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE INDEX cache_expiry ON cache (expires_at);

-- Reliable event handoff (enterprise, later; schema frozen now so the
-- Journal trait boundary stays stable).
CREATE TABLE outbox (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    flow_id     TEXT REFERENCES flows (flow_id),
    destination TEXT NOT NULL,
    payload     BLOB NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending','sent','failed'))
);
