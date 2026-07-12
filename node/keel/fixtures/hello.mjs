// Deterministic fixture app for the KEEL_DISABLE byte-identity test.
// No network, fixed output on both stdout and stderr, non-zero exit code.
process.stdout.write("stdout-line-1\n");
process.stderr.write("stderr-line-1\n");
console.log("computed", 40 + 2);
process.exit(7);
