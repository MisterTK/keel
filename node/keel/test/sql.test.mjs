// Shared SQL-verb idempotency judgment (sql.mjs), used by both the pg and
// mysql2 packs.

import test from "node:test";
import assert from "node:assert/strict";
import { sqlVerb, isIdempotentSql } from "../src/packs/sql.mjs";

test("sqlVerb extracts the leading statement keyword, uppercased", () => {
  assert.equal(sqlVerb("select 1"), "SELECT");
  assert.equal(sqlVerb("SELECT * FROM t"), "SELECT");
  assert.equal(sqlVerb("  \n  select 1"), "SELECT");
  assert.equal(sqlVerb("insert into t values (1)"), "INSERT");
  assert.equal(sqlVerb("UPDATE t SET x = 1"), "UPDATE");
  assert.equal(sqlVerb("DELETE FROM t"), "DELETE");
  assert.equal(sqlVerb("WITH x AS (SELECT 1) SELECT * FROM x"), "WITH");
});

test("sqlVerb skips leading line and block comments", () => {
  assert.equal(sqlVerb("-- a comment\nSELECT 1"), "SELECT");
  assert.equal(sqlVerb("-- one\n-- two\nSELECT 1"), "SELECT");
  assert.equal(sqlVerb("/* block */ SELECT 1"), "SELECT");
  assert.equal(sqlVerb("/* multi\nline */\n-- and a line comment\nSELECT 1"), "SELECT");
});

test("sqlVerb returns null for non-string, empty, or comment-only text", () => {
  assert.equal(sqlVerb(null), null);
  assert.equal(sqlVerb(undefined), null);
  assert.equal(sqlVerb(42), null);
  assert.equal(sqlVerb(""), null);
  assert.equal(sqlVerb("   "), null);
  assert.equal(sqlVerb("-- only a comment"), null);
  assert.equal(sqlVerb("/* only a comment */"), null);
});

test("isIdempotentSql: only a bare SELECT is retryable", () => {
  assert.equal(isIdempotentSql("SELECT 1"), true);
  assert.equal(isIdempotentSql("select * from t where id = $1"), true);
  assert.equal(isIdempotentSql("-- comment\nSELECT 1"), true);

  assert.equal(isIdempotentSql("INSERT INTO t VALUES (1)"), false);
  assert.equal(isIdempotentSql("UPDATE t SET x = 1"), false);
  assert.equal(isIdempotentSql("DELETE FROM t"), false);
  assert.equal(isIdempotentSql("CREATE TABLE t (id int)"), false);
  // A WITH CTE may wrap a data-modifying statement (a write disguised as a
  // read) — conservatively never treated as idempotent.
  assert.equal(isIdempotentSql("WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x"), false);
  assert.equal(isIdempotentSql(null), false);
  assert.equal(isIdempotentSql(""), false);
});
