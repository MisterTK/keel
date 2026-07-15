# keelrun

The Rust front end for [Keel](https://github.com/MisterTK/keel) — resilience
(retry/backoff/timeout/breaker/rate/cache) as a library, policy in one
`keel.toml`, no service to run.

Rust has no import hooks to hang a zero-code-change promise off of, so this
crate keeps the smallest possible ceiling instead: one attribute macro, plus
a `reqwest-middleware` adapter for outbound HTTP.

## Install

Published on crates.io as `keelrun` (plain `keel` is taken). Add it with an
explicit rename so `#[keel::wrap]` resolves — this is the intended way to
add it, mirroring how this project's own crates already depend on
`keelrun-core` as `keel-core`:

```bash
cargo add keelrun --rename keel
```

which is equivalent to adding this to `Cargo.toml` by hand:

```toml
[dependencies]
keel = { package = "keelrun", version = "0.1" }
```

## Quickstart

```rust,no_run
# use thiserror::Error;
# #[derive(Debug, Error)]
# #[error("upstream unavailable: {0}")]
# struct UpstreamError(String);
#[keel::wrap(target = "orders-api")]
async fn fetch_order(id: u64) -> Result<Order, UpstreamError> {
    // your existing code, unmodified — the attribute routes it through
    // Keel's cache -> rate -> breaker -> timeout -> retry chain
    todo!()
}
# #[derive(serde::Serialize, serde::Deserialize)]
# struct Order;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    keel::init()?; // reads ./keel.toml, or Level 0 defaults if absent
    let order = fetch_order(42).await?;
    # let _ = order;
    Ok(())
}
```

Outbound HTTP via `reqwest`:

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
keel::init()?;
let raw = reqwest::Client::new();
let client = reqwest_middleware::ClientBuilder::new(raw.clone())
    .with(keel::KeelMiddleware::new(raw))
    .build();
let resp = client.get("https://api.example.com/orders").send().await?;
# let _ = resp;
# Ok(())
# }
```

`#[keel::wrap]`'s v1 scope: a free (no `self`) `async fn` returning
`Result<T, E>` where `T: Serialize + DeserializeOwned` and
`E: std::error::Error + Send + Sync + 'static`; every parameter must
implement `Clone` (a retried call re-invokes the body, cloning fresh
arguments per attempt); the target string is explicit only, no signature
inference. `KeelMiddleware`'s v1 scope: exact-host targets only (no
`host:`/URL-pattern globs), no response caching, add it last in the
`ClientBuilder` chain. Both documented in full in each type's own rustdoc.

## Learn more

- [Root README](https://github.com/MisterTK/keel#readme) — what Keel is,
  the two-tier resilience/durability model, demos.
- [`docs/dx-spec.md`](https://github.com/MisterTK/keel/blob/main/docs/dx-spec.md) /
  [`docs/architecture-spec.md`](https://github.com/MisterTK/keel/blob/main/docs/architecture-spec.md)
  — the full design.
- The `keel` CLI (`doctor`/`init`/`status`/`mcp`/…) is a separate package,
  [`keelrun-cli`](https://crates.io/crates/keelrun-cli) — it does not yet
  scan Rust projects (`crates/keel/src/lib.rs`'s crate docs track this as
  known debt).

Licensed under Apache-2.0.
