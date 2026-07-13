/**
 * `keel/testing` — offline replay for a `keel record` capture.
 *
 *     import { withReplay } from "keel/testing";
 *
 *     test("my recorded flow", async () => {
 *       await withReplay("./recording.ndjson", async () => {
 *         // exercise the code you recorded; every intercepted effect it
 *         // makes is served from the recording, and an unrecorded/novel
 *         // effect throws UnmatchedEffectError instead of running live.
 *       });
 *     });
 *
 * See src/testing.mjs and docs/recording-format.md for the full design and
 * the request-matching rule.
 */

export {
  Recording,
  ReplayBackend,
  UnmatchedEffectError,
  installReplay,
  withReplay,
} from "./src/testing.mjs";
