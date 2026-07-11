-- Golden fixture: a flow that ran to completion.
-- Apply over contracts/journal.sql. Base timestamp T0 = 1783728000000
-- (2026-07-11T00:00:00Z). Payloads are MessagePack; decoded values noted.

INSERT INTO flows (flow_id, entrypoint, args_hash, code_hash, status,
                   lease_holder, lease_expires, created_at, updated_at)
VALUES ('01JZWY0A0000000000000001', 'py:pipeline.ingest:main',
        'ah-6c1f0a', 'ch-9b2e44', 'completed',
        NULL, NULL, 1783728000000, 1783728005400);

-- seq 1: fetch succeeded first try; payload {"rows": 120}
INSERT INTO steps VALUES ('01JZWY0A0000000000000001', 1,
        'api.source.internal#q1', 'effect', 1, 'ok',
        X'81A4726F777378', NULL, 1783728000010, 1783728000250);

-- seq 2: virtualized time read (time.time() inside the flow is an effect);
-- payload uint32 1783728000 (seconds)
INSERT INTO steps VALUES ('01JZWY0A0000000000000001', 2,
        'py:time.time#-', 'time', 1, 'ok',
        X'CE6A518600', NULL, 1783728000251, 1783728000251);

-- seq 3: enrich succeeded on attempt 2 (one policy retry, journaled as
-- attempts of ONE step); payload {"ok": true}
INSERT INTO steps VALUES ('01JZWY0A0000000000000001', 3,
        'api.enrich.internal#q2', 'effect', 2, 'ok',
        X'81A26F6BC3', NULL, 1783728000252, 1783728002700);

-- seq 4: store write; payload nil
INSERT INTO steps VALUES ('01JZWY0A0000000000000001', 4,
        'api.store.internal#w1', 'effect', 1, 'ok',
        X'C0', NULL, 1783728002701, 1783728004100);

-- seq 5: notify; payload nil
INSERT INTO steps VALUES ('01JZWY0A0000000000000001', 5,
        'api.notify.internal#n1', 'effect', 1, 'ok',
        X'C0', NULL, 1783728004101, 1783728005390);
