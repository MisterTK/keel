// Native harness for the shared conformance suite. Drives the napi addon
// (crates/keel-node) through the SAME driver as the pure-JS stub
// (node/keel-core-stub/test/driver.mjs), on the harness-only paused clock.
//
// If the addon binary is not built, this registers a single skipped test with a
// clear message so plain CI (which does not build the addon) stays green. To run
// it for real: `cargo build -p keel-node --release`, then `node --test test/`.

import test from "node:test";
import { KeelCore, loaded } from "../index.mjs";
import { registerConformance } from "../../keel-core-stub/conformance-driver.mjs";

if (!loaded || typeof KeelCore !== "function") {
  test("native conformance (addon not built)", {
    skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`",
  });
} else {
  registerConformance(test, {
    label: "native",
    makeCore: () => new KeelCore({ paused: true }),
    // The native core throws a plain JS Error carrying the stable `.code`.
    isKeelError: (e) => typeof e?.code === "string" && e.code.startsWith("KEEL-"),
  });
}
