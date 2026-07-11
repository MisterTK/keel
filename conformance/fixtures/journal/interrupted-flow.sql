-- Golden fixture: a flow interrupted mid-step (process crash), lease expired.
-- This is exactly the shape recovery scans for: status='running',
-- lease_expires in the past, last step outcome='running' with no result —
-- on resume, steps 1-3 replay from the journal and step 4 re-executes live.
-- Base timestamp T0 = 1783728000000 (2026-07-11T00:00:00Z).

INSERT INTO flows (flow_id, entrypoint, args_hash, code_hash, status,
                   lease_holder, lease_expires, created_at, updated_at)
VALUES ('01JZWY0A0000000000000002', 'py:pipeline.ingest:main',
        'ah-77d2b9', 'ch-9b2e44', 'running',
        'host-a:pid-4242', 1783728030000, 1783728000000, 1783728002000);

INSERT INTO steps VALUES ('01JZWY0A0000000000000002', 1,
        'api.source.internal#q1', 'effect', 1, 'ok',
        X'81A4726F777378', NULL, 1783728000010, 1783728000250);

INSERT INTO steps VALUES ('01JZWY0A0000000000000002', 2,
        'py:time.time#-', 'time', 1, 'ok',
        X'CE6A518600', NULL, 1783728000251, 1783728000251);

INSERT INTO steps VALUES ('01JZWY0A0000000000000002', 3,
        'api.enrich.internal#q2', 'effect', 1, 'ok',
        X'81A26F6BC3', NULL, 1783728000252, 1783728001400);

-- crash happened here: step started, never finished
INSERT INTO steps VALUES ('01JZWY0A0000000000000002', 4,
        'api.store.internal#w1', 'effect', 1, 'running',
        NULL, NULL, 1783728001401, NULL);
