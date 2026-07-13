/**
 * PINNED FIXTURE — mirrors the base (callback) `Connection.query`/`.execute`
 * seam of **mysql2@3.x**. The real `mysql2` package is NOT a dependency of
 * Keel; this is the frozen shape the `mysql2` pack patches
 * (`Connection.prototype.query`/`.execute`) and is contract-tested against.
 * `mysql2/promise`'s `PromiseConnection` calls these SAME base methods
 * internally (passing its own callback), so patching this one seam covers
 * both public APIs. If mysql2 bumps this boundary, update this fixture +
 * `PINNED_MAJOR` in `src/packs/mysql2.mjs` and re-certify (adapter-pack
 * contract: version-pinned tests).
 *
 * `test/mysql2.test.mjs` builds a plain JS fake conforming to this shape.
 */

export interface QueryOptions {
  sql: string;
  values?: unknown[];
}

export interface ConnectionConfig {
  host?: string;
}

/** A streaming command object returned by a CALLBACK-LESS call — an
 *  EventEmitter for row-by-row results (`.on('result', ...)`). The pack
 *  forwards a callback-less call untouched and never wraps this shape. */
export interface Query {
  on(event: "result" | "error" | "fields" | "end", listener: (...args: unknown[]) => void): this;
}

export type QueryCallback = (err: Error | null, results?: unknown, fields?: unknown) => void;

export interface Connection {
  /** Resolved connection options — the pack derives its target from
   *  `config.host`. */
  config: ConnectionConfig;

  /** With a trailing callback (the shape `mysql2/promise` always uses
   *  internally, and the one this pack wraps): dispatches the query and
   *  invokes `callback(err, results, fields)` exactly once.
   *  WITHOUT a callback: returns a streaming `Query` object. Forwarded
   *  untouched — see module docstring in `src/packs/mysql2.mjs`. */
  query(sql: string | QueryOptions, callback: QueryCallback): Query | undefined;
  query(sql: string | QueryOptions, values: unknown[], callback: QueryCallback): Query | undefined;
  query(sql: string | QueryOptions): Query;

  /** Prepared-statement form; same two call shapes as `query`. */
  execute(sql: string | QueryOptions, callback: QueryCallback): Query | undefined;
  execute(sql: string | QueryOptions, values: unknown[], callback: QueryCallback): Query | undefined;
  execute(sql: string | QueryOptions): Query;
}
