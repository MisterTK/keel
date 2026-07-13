/**
 * Shared SQL-verb idempotency judgment for database library packs (`pg.mjs`,
 * `mysql2.mjs`). One place so the (conservative) rule is written once and both
 * packs stay in lockstep.
 *
 * Rule (Level 0 hard rule, dx-spec §1: never retry non-idempotent calls by
 * default): a query is retryable ONLY when its statement is a bare `SELECT`.
 * Everything else — `INSERT`/`UPDATE`/`DELETE`, DDL, a `WITH` CTE (which may
 * wrap a data-modifying statement, e.g. `WITH x AS (DELETE ... RETURNING *)
 * SELECT * FROM x` — a write disguised as a read), and any unrecognized or
 * unparsable statement — is treated as non-idempotent: observed, not retried
 * (KEEL-E014). This is deliberately conservative: we never attempt to prove a
 * statement side-effect-free by parsing it fully, we only recognize the one
 * unambiguously-safe shape.
 */

/** Strip leading whitespace and SQL comments (`--...` line, `/* ... *\/` block)
 *  and return the first statement's leading keyword, uppercased, or `null` if
 *  none is found (empty/comment-only text). */
export function sqlVerb(text) {
  if (typeof text !== "string") return null;
  let s = text;
  // Repeatedly strip leading whitespace/comments — a query may open with
  // several comment lines before the statement itself.
  for (;;) {
    const trimmed = s.replace(/^\s+/, "");
    if (trimmed.startsWith("--")) {
      const nl = trimmed.indexOf("\n");
      s = nl === -1 ? "" : trimmed.slice(nl + 1);
      continue;
    }
    if (trimmed.startsWith("/*")) {
      const end = trimmed.indexOf("*/");
      s = end === -1 ? "" : trimmed.slice(end + 2);
      continue;
    }
    s = trimmed;
    break;
  }
  const m = /^([A-Za-z]+)/.exec(s);
  return m ? m[1].toUpperCase() : null;
}

/** True iff `text`'s leading statement is a bare `SELECT` — the only verb this
 *  pack ever treats as retry-safe. */
export function isIdempotentSql(text) {
  return sqlVerb(text) === "SELECT";
}
