-- The statements the plan-postgres-*.txt fixtures in this directory were captured with.
--
-- Database: a scratch `dbdelve_plan_fixtures` on the compose Postgres (15.13), built by
-- dev/load/postgres.sql with @ROWS@ = 1000000 and `ANALYZE events`, then dropped. The
-- committed fixtures carry only what these statements and that generated table produce.
-- Capture: one statement per `psql -XqAt -d dbdelve_plan_fixtures -f <file>` run, so each
-- starts with a cold catalog cache (which is why `Planning:` carries a `Buffers:` line)
-- and no `SET` echo becomes a root node. The file holds the plan rows as the server
-- returned them.

-- plan-postgres-parallel.txt: a parallel aggregate under Gather Merge, workers at loops=3.
SET work_mem = '1MB';
SET max_parallel_workers_per_gather = 2;
EXPLAIN (ANALYZE, BUFFERS)
SELECT account_id, kind, count(*) AS n, sum(amount) AS total
FROM events WHERE amount > 100 GROUP BY account_id, kind ORDER BY total DESC LIMIT 50;

-- plan-postgres-cte-subplan.txt: a materialized CTE read twice, one a correlated SubPlan,
-- with the JIT thresholds at 0 so a `JIT:` block is there whatever the scratch data costs.
SET work_mem = '1MB';
SET max_parallel_workers_per_gather = 2;
SET jit = on;
SET jit_above_cost = 0;
SET jit_inline_above_cost = 0;
SET jit_optimize_above_cost = 0;
EXPLAIN (ANALYZE, BUFFERS)
WITH big AS MATERIALIZED (SELECT account_id, kind, amount, id FROM events WHERE amount > 50)
SELECT b.account_id, count(*), sum(b.amount),
       (SELECT count(*) FROM big x WHERE x.account_id = b.account_id AND x.id % 97 = 0)
FROM big b JOIN (SELECT DISTINCT account_id AS id FROM events) a ON a.id = b.account_id
GROUP BY b.account_id ORDER BY 3 DESC;

-- plan-postgres-spill.txt: an external-merge sort under Limit, and a Limit over a Seq Scan
-- that stops early (a misestimate that is not one).
SET work_mem = '1MB';
SET max_parallel_workers_per_gather = 2;
EXPLAIN (ANALYZE, BUFFERS)
SELECT e.id, e.amount FROM events e
WHERE e.kind IN (SELECT kind FROM events WHERE account_id = 3 LIMIT 1)
ORDER BY e.amount DESC, e.id OFFSET 100000 LIMIT 10;

-- plan-postgres-large.txt: Gather Merge, a `CTE big` heading and an external-merge sort in
-- one plan, 58 nodes. The first of the three candidate statements gave all three shapes
-- (a top-level materialized CTE read twice, under a UNION ALL of parallel and serial branches).
SET work_mem = '1MB';
SET max_parallel_workers_per_gather = 2;
EXPLAIN (ANALYZE, BUFFERS)
WITH big AS MATERIALIZED (SELECT account_id, kind, amount, id FROM events WHERE amount > 50)
SELECT 'a'::text AS src, account_id::bigint AS k, count(*)::bigint AS n, sum(amount) AS v
  FROM events WHERE amount > 100 GROUP BY account_id, kind
UNION ALL
SELECT 'b', b.account_id, count(*), sum(b.amount)
  FROM big b JOIN (SELECT DISTINCT account_id AS id FROM events) a ON a.id = b.account_id
  GROUP BY b.account_id
UNION ALL
SELECT 'b2', x.account_id, count(*), max(x.amount) FROM big x WHERE x.kind = 'click' GROUP BY x.account_id
UNION ALL
SELECT 'c', s.id, 0, s.amount
  FROM (SELECT e.id, e.amount FROM events e
        WHERE e.kind IN (SELECT kind FROM events WHERE account_id = 3 LIMIT 1)
        ORDER BY e.amount DESC, e.id OFFSET 100000 LIMIT 10) s
UNION ALL SELECT 'k1', account_id, count(*), avg(amount) FROM events WHERE kind = 'click' AND amount > 200 GROUP BY account_id
UNION ALL SELECT 'k2', account_id, count(*), avg(amount) FROM events WHERE kind = 'view' AND amount > 200 GROUP BY account_id
UNION ALL SELECT 'k3', account_id, count(*), avg(amount) FROM events WHERE kind = 'purchase' AND amount > 200 GROUP BY account_id
UNION ALL SELECT 'k4', account_id, count(*), avg(amount) FROM events WHERE kind = 'refund' AND amount > 200 GROUP BY account_id
UNION ALL SELECT 'k5', account_id, count(*), avg(amount) FROM events WHERE kind = 'signup' AND amount > 200 GROUP BY account_id
UNION ALL SELECT 'k6', account_id, count(*), avg(amount) FROM events WHERE kind = 'churn' AND amount > 200 GROUP BY account_id;
