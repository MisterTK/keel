// A tiny script that calls an HTTP API with fetch.
const DATA_API = "https://api.example.com/v1/data";

export async function fetchData() {
  const res = await fetch(DATA_API);
  return res.json();
}
