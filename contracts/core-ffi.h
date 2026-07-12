/* keel — core FFI surface, contracts-v1.
 *
 * This header is the frozen C ABI between keel-core (Rust) and the language
 * front ends (PyO3 / napi-rs bindings wrap this same surface; the CLI links
 * the core directly). Changes require an approved contract-change request.
 *
 * Envelope: requests and outcomes cross the boundary as versioned,
 * self-describing MessagePack maps whose logical shape is defined in
 * contracts/core_api.rs (the serde types there are normative; MessagePack is
 * the wire encoding of exactly those shapes, field names as map keys).
 * Every envelope carries "v": 1. Unknown fields MUST be ignored by readers;
 * an unsupported "v" fails with KEEL_E004.
 *
 * Threading: a KeelCore handle is internally synchronized; all functions are
 * callable from any thread. keel_execute may block (backoff waits, rate
 * queueing); front ends run it on an appropriate executor. The async bridge
 * (PyO3-async / napi tokio) is built on keel_execute_start/poll in Sprint 1
 * by Team A and conformance-tested as part of the FFI surface — the
 * synchronous form below is the contract every binding must support.
 */

#ifndef KEEL_CORE_FFI_H
#define KEEL_CORE_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define KEEL_ABI_VERSION 1
#define KEEL_ENVELOPE_VERSION 1 /* the "v" field inside every envelope */

/* ------------------------------------------------------------------ */
/* Error taxonomy. Stable codes; the string form "KEEL-E0NN" appears in
 * envelopes, logs, and `keel explain <code>`. Numeric values are frozen. */
typedef enum KeelErrorCode {
  KEEL_OK = 0,

  /* configuration / envelope */
  KEEL_E001_POLICY_INVALID = 1,        /* policy document failed validation   */
  KEEL_E002_TARGET_UNKNOWN = 2,        /* reserved: strict-mode unknown target */
  KEEL_E003_ENVELOPE_DECODE = 3,       /* request/outcome failed to decode    */
  KEEL_E004_ENVELOPE_VERSION = 4,      /* unsupported envelope "v"            */
  KEEL_E005_UNSUPPORTED_CONFIGURATION = 5, /* policy is valid but requests something
                                          this build/configuration cannot provide */

  /* resilience-layer terminal outcomes */
  KEEL_E010_ATTEMPTS_EXHAUSTED = 10,   /* retryable failure on final attempt  */
  KEEL_E011_TIMEOUT = 11,              /* call exceeded policy timeout        */
  KEEL_E012_BREAKER_OPEN = 12,         /* breaker open: failed fast, no call  */
  KEEL_E013_RATE_BUDGET_EXCEEDED = 13, /* reserved: rate queue budget blown   */
  KEEL_E014_NON_IDEMPOTENT_NOT_RETRIED = 14, /* retryable error, unsafe call:
                                          observed, not retried (Level 0 rule) */
  KEEL_E015_NON_RETRYABLE_ERROR = 15,  /* error class not in retry.on         */
  KEEL_E020_CACHE_CODEC = 20,          /* reserved: unserializable cache value */

  /* flow / journal (Tier 2; reserved in v0.1) */
  KEEL_E030_FLOW_LEASE_HELD = 30,
  KEEL_E031_FLOW_NONDETERMINISM = 31,
  KEEL_E032_FLOW_DEAD = 32,

  KEEL_E040_INTERNAL = 40
} KeelErrorCode;

/* ------------------------------------------------------------------ */
/* Buffers. Core-allocated output buffers are returned via KeelBuf and MUST
 * be released with keel_buf_free exactly once. */
typedef struct KeelBuf {
  uint8_t *data;
  size_t len;
} KeelBuf;

void keel_buf_free(KeelBuf buf);

/* Opaque core handle. */
typedef struct KeelCore KeelCore;

/* ------------------------------------------------------------------ */
/* Lifecycle */

/* Create a core. Never fails except OOM (returns NULL). */
KeelCore *keel_new(void);

void keel_free(KeelCore *core);

/* Configure (or reconfigure) with a policy document: the keel.toml content
 * parsed to JSON (UTF-8), matching contracts/policy.schema.json.
 *
 * The document passed here is the EFFECTIVE policy: composition of the built-in
 * smart-defaults pack (contracts/defaults.toml), any adapter-pack defaults, and
 * the user's keel.toml — precedence defaults < packs < user, per-layer wholesale
 * replacement (the merge rule in contracts/defaults.toml) — is the job of the
 * language front ends and the CLI, which perform it BEFORE calling this
 * function. The core interprets exactly the document it is given (no pack is
 * layered underneath), which is what the conformance corpus normatively assumes:
 * its scenarios drive this entry point with bare policies.
 *
 * Returns KEEL_OK or KEEL_E001; on error *err_out (if non-NULL) receives a
 * UTF-8 JSON diagnostic {code, message}. */
int32_t keel_configure(KeelCore *core, const uint8_t *policy_json,
                       size_t policy_len, KeelBuf *err_out);

/* ------------------------------------------------------------------ */
/* Execution */

/* The effect callback: performs one attempt of the underlying call in the
 * host language. `request` is the AttemptRequest envelope (MessagePack);
 * the callback writes an AttemptResult envelope into *result_out (allocated
 * by the callee with its own allocator; the core copies before return).
 * Return 0 on a produced result (success OR typed error — a failed HTTP call
 * is a result, not a callback failure); nonzero only if the callback itself
 * could not run (treated as error class "other"). */
typedef int32_t (*keel_effect_fn)(void *userdata, uint32_t attempt,
                                  const uint8_t *request, size_t request_len,
                                  KeelBuf *result_out);

/* Execute one intercepted call through the target's compiled layer chain
 * (cache -> rate -> breaker -> timeout -> retry -> idempotency -> journal).
 * `request` is the Request envelope (MessagePack per core_api.rs).
 * Always produces an Outcome envelope in *outcome_out, including on policy
 * failures — the original last error payload rides inside the Outcome so
 * front ends re-raise the ORIGINAL exception unchanged (DX invariant 5).
 * Return value is KEEL_OK unless the envelope itself was undecodable
 * (KEEL_E003/E004) or internal failure (KEEL_E040). */
int32_t keel_execute(KeelCore *core, const uint8_t *request,
                     size_t request_len, keel_effect_fn effect,
                     void *userdata, KeelBuf *outcome_out);

/* ------------------------------------------------------------------ */
/* Reporting */

/* Metrics/discovery report as deterministic UTF-8 JSON (sorted keys, no
 * wall-clock timestamps): per-target counters (calls, attempts, retries,
 * successes, failures, cache_hits, throttled, breaker_opens, breaker_state).
 * Feeds `keel status`, `keel doctor`, and .keel/discovery.db. */
int32_t keel_report(KeelCore *core, KeelBuf *json_out);

#ifdef __cplusplus
}
#endif

#endif /* KEEL_CORE_FFI_H */
