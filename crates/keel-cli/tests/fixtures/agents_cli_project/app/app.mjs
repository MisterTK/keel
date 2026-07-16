// A tiny agents-cli agent, calling an HTTP API with fetch — a stand-in for
// what `agents-cli scaffold create` generates under its `agent_directory`.
// The generated Dockerfile COPYs this directory into the image; the project
// root (one level up, where agents-cli-manifest.yaml lives) is not. JS scan
// is pure Rust (no python3), so tests built on this fixture stay
// deterministic without an interpreter on PATH.
const DATA_API = "https://api.example.com/v1/data";

export async function fetchData() {
  const res = await fetch(DATA_API);
  return res.json();
}
