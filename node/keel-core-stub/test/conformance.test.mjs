// Node harness for the shared conformance suite (conformance/scenarios/),
// driving the pure-JS keel-core-stub. The scenario format, execution semantics,
// and the same suite driven against the native napi addon all live in
// ./driver.mjs (consumed here and by node/keel-core-native/test). Normative
// semantics: conformance/README.md.

import test from "node:test";
import { KeelCoreStub, KeelError } from "../index.mjs";
import { registerConformance } from "../conformance-driver.mjs";

registerConformance(test, {
  label: "stub",
  makeCore: () => new KeelCoreStub(),
  isKeelError: (e) => e instanceof KeelError,
});
