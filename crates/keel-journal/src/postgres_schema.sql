-- keel — journal schema, Postgres flavor of contracts/journal.sql.
--
-- This is NOT the frozen contract (that's SQLite-only, contracts/journal.sql,
-- vendored verbatim into keel-journal/contract/). It mirrors the same tables,
-- columns, and CHECK sets so a Postgres-backed journal is semantically
-- identical to the SQLite one at the row level; only the SQL dialect differs
-- (BYTEA instead of BLOB, BIGSERIAL instead of AUTOINCREMENT, no
-- `WITHOUT ROWID`/`PRAGMA`). All timestamps are integer milliseconds since the
-- Unix epoch, matching the contract.
--
-- Applied idempotently (`IF NOT EXISTS` throughout, `ON CONFLICT DO NOTHING`
-- for the seed row) inside a `pg_advisory_lock`-guarded batch — see
-- `postgres::init_schema` — so concurrent first-connects across a fleet race
-- safely instead of failing with "relation already exists".

CREATE TABLE IF NOT EXISTS keel_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

INSERT INTO keel_meta (key, value) VALUES ('schema_version', '1')
    ON CONFLICT (key) DO NOTHING;

-- One row per flow execution. Identity: (entrypoint, args_hash, explicit key).
CREATE TABLE IF NOT EXISTS flows (
    flow_id       TEXT PRIMARY KEY,             -- ULID
    entrypoint    TEXT NOT NULL,                -- e.g. 'py:pipeline.ingest:main'
    args_hash     TEXT NOT NULL,
    code_hash     TEXT,                         -- fences deploys during replay
    status        TEXT NOT NULL
                  CHECK (status IN ('running','completed','failed','dead')),
    lease_holder  TEXT,                         -- process id, NULL when unleased
    lease_expires BIGINT,                       -- ms epoch, NULL when unleased
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL
);

-- Recovery scan: incomplete flows with expired leases.
CREATE INDEX IF NOT EXISTS flows_incomplete ON flows (status, lease_expires);

-- One row per journaled step. Step identity within a flow: seq (execution
-- order); replay matches on (seq, step_key) and any divergence is
-- nondeterminism (KEEL-E031, handled per flows.on_nondeterminism policy).
CREATE TABLE IF NOT EXISTS steps (
    flow_id     TEXT    NOT NULL REFERENCES flows (flow_id),
    seq         BIGINT  NOT NULL,
    step_key    TEXT    NOT NULL,               -- '(target)#(args_hash)'
    kind        TEXT    NOT NULL
                CHECK (kind IN ('effect','time','random','subprocess','marker')),
    attempt     BIGINT  NOT NULL DEFAULT 1,     -- attempts consumed by this step
    outcome     TEXT    NOT NULL
                CHECK (outcome IN ('ok','error','running')),
    payload     BYTEA,                          -- MessagePack, schema-tagged
    error_class TEXT,                           -- ErrorClass when outcome='error'
    started_at  BIGINT  NOT NULL,
    ended_at    BIGINT,                         -- NULL while running
    PRIMARY KEY (flow_id, seq)
);

CREATE INDEX IF NOT EXISTS steps_by_key ON steps (flow_id, step_key);

-- Persistent response cache (cache = { scope = "persistent" }).
CREATE TABLE IF NOT EXISTS cache (
    key        TEXT PRIMARY KEY,                -- '(target)#(args_hash)'
    value      BYTEA NOT NULL,                  -- MessagePack, schema-tagged
    expires_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS cache_expiry ON cache (expires_at);

-- Reliable event handoff (enterprise, later; schema frozen now so the
-- Journal trait boundary stays stable).
CREATE TABLE IF NOT EXISTS outbox (
    id          BIGSERIAL PRIMARY KEY,
    flow_id     TEXT REFERENCES flows (flow_id),
    destination TEXT NOT NULL,
    payload     BYTEA NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending','sent','failed'))
);
