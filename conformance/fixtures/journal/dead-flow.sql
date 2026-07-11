-- Golden fixture: a poison flow — failed on every resume attempt, marked
-- dead after the resume cap. Surfaced by `keel flows --dead`; the terminal
-- step keeps its error for `keel trace`.
-- Base timestamp T0 = 1783728000000 (2026-07-11T00:00:00Z); the flow was
-- created a day earlier and died at T0.

INSERT INTO flows (flow_id, entrypoint, args_hash, code_hash, status,
                   lease_holder, lease_expires, created_at, updated_at)
VALUES ('01JZWY0A0000000000000003', 'py:jobs.nightly:run',
        'ah-04c811', 'ch-1a7f02', 'dead',
        NULL, NULL, 1783641600000, 1783728000000);

INSERT INTO steps VALUES ('01JZWY0A0000000000000003', 1,
        'api.source.internal#q9', 'effect', 1, 'ok',
        X'81A4726F777378', NULL, 1783641600010, 1783641600300);

-- the poison step: 5 attempts consumed on the final resume, still failing
INSERT INTO steps VALUES ('01JZWY0A0000000000000003', 2,
        'api.billing.internal#w7', 'effect', 5, 'error',
        NULL, 'http', 1783641600301, 1783728000000);
