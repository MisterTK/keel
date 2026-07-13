/**
 * PINNED FIXTURE — mirrors the `Client.query` seam of **pg@8.x**
 * (node-postgres). The real `pg` package is NOT a dependency of Keel; this is
 * the frozen shape the `pg` pack patches (`Client.prototype.query`) and is
 * contract-tested against. If pg bumps this boundary, update this fixture +
 * `PINNED_MAJOR` in `src/packs/pg.mjs` and re-certify (adapter-pack contract:
 * version-pinned tests).
 *
 * `test/pg.test.mjs` builds a plain JS fake conforming to this shape.
 */

export interface PgQueryConfig {
  text: string;
  values?: unknown[];
  name?: string;
  rowMode?: "array";
}

/** Anything with a `.submit` method (a raw `Query`, `pg-cursor`,
 *  `pg-query-stream`) — `client.query` hands these back UNCHANGED, they are
 *  not a request/response the pack can retry. */
export interface PgSubmittable {
  submit(connection: unknown): void;
}

export interface PgResult {
  rows: unknown[];
  rowCount: number;
}

export interface PgConnectionParameters {
  /** Resolved from `host`/`connectionString`; what the pack derives the
   *  target from. */
  host?: string;
}

export interface PgClient {
  /** Parsed connection config — `connectionParameters.host` is the pack's
   *  target derivation (docs/pages/apis/client.mdx / connecting.mdx). */
  connectionParameters?: PgConnectionParameters;

  /** The single query-dispatch chokepoint. Three documented call shapes:
   *  - no callback → returns a real `Promise<PgResult>` (the shape this pack
   *    wraps).
   *  - a callback (any arity) → returns `undefined`, flow control is the
   *    callback's alone (pg's upgrading guide). Forwarded untouched.
   *  - a `PgSubmittable` first argument → returns it unchanged (its own
   *    event-emitter protocol). Forwarded untouched. */
  query(text: string | PgQueryConfig, values?: unknown[]): Promise<PgResult>;
  query(text: string | PgQueryConfig, callback: (err: Error | null, result?: PgResult) => void): undefined;
  query(text: string | PgQueryConfig, values: unknown[], callback: (err: Error | null, result?: PgResult) => void): undefined;
  query<T extends PgSubmittable>(submittable: T): T;
}
