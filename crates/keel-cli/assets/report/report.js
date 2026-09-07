// keel report page (design spec 2026-09-04, Part C). Vanilla JS, no network
// except the same-origin poll in serve mode. `viewModel` is pure and is
// unit-tested under `node --test` (node/keel/test/report-page.test.mjs);
// `render` patches the DOM idempotently. Both are published on
// `globalThis.KeelReport`; the boot code runs only when a document exists.
(function () {
  "use strict";

  var DAY_MS = 86400000;
  var EVENT_CAP = 200;
  var POLL_MS = 1000;

  function clock(ms) {
    var m = Math.floor(ms / 60000), s = Math.floor((ms % 60000) / 1000), f = ms % 1000;
    return (m < 10 ? "0" : "") + m + ":" + (s < 10 ? "0" : "") + s + "." + ("00" + f).slice(-3);
  }
  function wait(ms) {
    if (ms < 1000) return ms + "ms";
    if (ms % 1000 === 0) return (ms / 1000) + "s";
    return Math.floor(ms / 1000) + "." + Math.floor((ms % 1000) / 100) + "s";
  }
  function attempts(n) { return n === 1 ? "1 attempt" : n + " attempts"; }
  function pad(s, w) { s = String(s); while (s.length < w) s += " "; return s; }
  function pct(x) { return (Number(x || 0) * 100).toFixed(1) + "%"; }

  // One feed line in `keel tail`'s vocabulary; null when there is no
  // ms/event envelope; unknown kinds degrade to a generic line.
  function eventLine(e) {
    if (!e || typeof e.ms !== "number" || typeof e.event !== "string") return null;
    var time = clock(e.ms);
    if (e.event === "run_start") return time + "  run " + (e.run || "?") + (e.pid ? " (pid " + e.pid + ")" : "");
    var call = e.call || "-", target = e.target || "-", verb, detail = "";
    switch (e.event) {
      case "call_start": verb = "call"; detail = e.op || ""; break;
      case "cache_hit": verb = "cache"; detail = "hit (" + (e.scope || "?") + ")"; break;
      case "cache_miss": verb = "cache"; detail = "miss (" + (e.scope || "?") + ")"; break;
      case "throttle": verb = "rate"; detail = "queued " + wait(e.wait_ms || 0); break;
      case "breaker_reject": verb = "breaker"; detail = "rejected — open, failing fast"; break;
      case "breaker_half_open": verb = "breaker"; detail = "half-open — probing"; break;
      case "breaker_open": verb = "breaker"; detail = "opened (cooldown " + wait(e.cooldown_ms || 0) + ")"; break;
      case "breaker_close": verb = "breaker"; detail = "closed"; break;
      case "attempt_start": verb = "attempt"; detail = "#" + (e.attempt || 0); break;
      case "attempt_error": verb = "fail"; detail = "#" + (e.attempt || 0) + " " + (e.class || "?") + (e.http_status ? " " + e.http_status : ""); break;
      case "backoff": verb = "backoff"; detail = wait(e.wait_ms || 0) + " → #" + ((e.attempt || 0) + 1); break;
      case "call_end":
        if (e.result === "ok") { verb = "ok"; detail = attempts(e.attempts || 0); }
        else { verb = "error"; detail = (e.code || "error") + " after " + attempts(e.attempts || 0); }
        break;
      default: verb = e.event;
    }
    return (time + "  " + pad(call, 9) + " " + pad(target, 24) + " " + pad(verb, 8) + " " + detail).replace(/\s+$/, "");
  }

  function bannerText(state) {
    if (state.mode === "serve") return "Live";
    if (state.mode === "watch") return "Live via --watch (reload every " + Math.round((state.watch_interval_ms || 2000) / 1000) + "s)";
    var t = new Date(state.generated_at_ms || 0);
    return "Snapshot at " + t.toLocaleTimeString() + " · regenerate with `keel report`, or `keel report --serve` for live";
  }

  // Pure: blob → plain data the DOM patcher consumes.
  function viewModel(state) {
    var s = state.status || {};
    var week = s.week || {};
    var headline = [
      { label: "calls", value: s.calls || 0, sub: (week.calls || 0) + " this week" },
      { label: "absorbed rate limits", value: s.throttled || 0 },
      { label: "retries", value: s.retries || 0, sub: (week.retries || 0) + " this week" },
      { label: "breaker trips", value: s.breaker_opens || 0 },
      { label: "cache hit rate", value: pct(s.cache_hit_rate) },
      { label: "unprotected calls", value: s.unwrapped_calls || 0, flag: (s.unwrapped_calls || 0) > 0 },
      { label: "not retried", value: s.not_retried || 0, flag: (s.not_retried || 0) > 0 }
    ];
    var rows = (s.targets || []).map(function (t) {
      return {
        key: t.target,
        cells: [t.target, t.calls, t.unwrapped_calls, t.retries, t.not_retried, t.throttled, t.breaker_opens, pct(t.calls ? t.cache_hits / t.calls : 0)],
        flags: { unwrapped: t.unwrapped_calls > 0, notRetried: t.not_retried > 0 }
      };
    });
    var byDay = {};
    (state.daily || []).forEach(function (d) {
      var b = byDay[d.day] || (byDay[d.day] = { day: d.day, calls: 0, retries: 0, failures: 0 });
      b.calls += d.calls || 0; b.retries += d.retries || 0; b.failures += d.failures || 0;
    });
    var endDay = Math.floor((state.generated_at_ms || 0) / DAY_MS);
    var days = [];
    for (var i = 6; i >= 0; i--) {
      var day = endDay - i;
      days.push(byDay[day] || { day: day, calls: 0, retries: 0, failures: 0 });
    }
    var flows = s.flows && s.flows.total > 0 ? s.flows : null;
    var events = (state.events || []).map(eventLine).filter(function (l) { return l !== null; });
    return { headline: headline, rows: rows, days: days, flows: flows, events: events, banner: bannerText(state) };
  }

  // ---- DOM patching (idempotent) ----
  // Keyed by target name; a null prototype so a target literally named
  // `constructor` or `__proto__` cannot collide with Object.prototype.
  var rowNodes = Object.create(null);
  function setText(el, text) { if (el.textContent !== String(text)) el.textContent = text; }

  function renderHeadline(vm) {
    var host = document.getElementById("headline");
    var tiles = host.children;
    vm.headline.forEach(function (h, i) {
      var tile = tiles[i];
      if (!tile) {
        tile = document.createElement("div");
        tile.innerHTML = '<div class="v"></div><div class="l"></div><div class="s"></div>';
        host.appendChild(tile);
      }
      tile.className = "tile" + (h.flag ? " flag" : "");
      setText(tile.children[0], h.value);
      setText(tile.children[1], h.label);
      setText(tile.children[2], h.sub || "");
    });
  }

  function renderRows(vm) {
    var body = document.querySelector("#targets tbody");
    var seen = {};
    vm.rows.forEach(function (r, i) {
      var tr = rowNodes[r.key];
      if (!tr) {
        tr = document.createElement("tr");
        r.cells.forEach(function () { tr.appendChild(document.createElement("td")); });
        rowNodes[r.key] = tr;
      }
      r.cells.forEach(function (c, j) { setText(tr.children[j], c); });
      tr.children[2].className = r.flags.unwrapped ? "flag" : "";
      tr.children[4].className = r.flags.notRetried ? "flag" : "";
      if (body.children[i] !== tr) body.insertBefore(tr, body.children[i] || null);
      seen[r.key] = true;
    });
    Object.keys(rowNodes).forEach(function (k) {
      if (!seen[k]) { rowNodes[k].remove(); delete rowNodes[k]; }
    });
  }

  function renderTrend(vm) {
    var svg = document.getElementById("trend");
    var max = Math.max.apply(null, vm.days.map(function (d) { return d.calls; }).concat([1]));
    var out = "";
    vm.days.forEach(function (d, i) {
      var x = 10 + i * 98, h = Math.round((d.calls / max) * 80), fh = Math.round((d.failures / max) * 80);
      var label = new Date(d.day * DAY_MS).toISOString().slice(5, 10);
      out += '<rect class="calls" x="' + x + '" y="' + (90 - h) + '" width="60" height="' + h + '"></rect>';
      if (fh > 0) out += '<rect class="failures" x="' + x + '" y="' + (90 - fh) + '" width="60" height="' + fh + '"></rect>';
      out += '<text x="' + (x + 30) + '" y="108" text-anchor="middle">' + label + '</text>';
      out += '<text x="' + (x + 30) + '" y="' + (86 - h) + '" text-anchor="middle">' + d.calls + '</text>';
    });
    if (svg.innerHTML !== out) svg.innerHTML = out;
  }

  function renderFlows(vm) {
    var section = document.getElementById("flows-section");
    if (!vm.flows) { section.hidden = true; return; }
    section.hidden = false;
    var dl = document.getElementById("flows");
    var parts = ["total", "running", "completed", "failed", "resumable", "dead"];
    if (dl.children.length !== parts.length * 2) {
      dl.innerHTML = parts.map(function (p) { return "<div><dt>" + p + "</dt><dd></dd></div>"; }).join("");
    }
    parts.forEach(function (p, i) { setText(dl.children[i].children[1], vm.flows[p] || 0); });
  }

  function render(state) {
    var vm = viewModel(state);
    setText(document.getElementById("banner"), vm.banner);
    renderHeadline(vm);
    renderRows(vm);
    renderTrend(vm);
    renderFlows(vm);
    setText(document.getElementById("events"), vm.events.slice(-EVENT_CAP).join("\n"));
  }

  function setStalled(on) {
    var b = document.getElementById("banner");
    b.className = "banner" + (on ? " stalled" : "");
    if (on) setText(b, "Live — stalled (server not responding)");
  }

  // Pure: fold one `/api/state` response into the poll cursor. `cursor` is
  // `{ since, runId, events }`; `since === null` means "no cursor yet" and
  // asks the server for the whole run (seq 0 included — `since` is
  // exclusive server-side, so an initial `?since=0` would drop `run_start`).
  //
  // A run change is detected by `state.run.id !== cursor.runId`. The first
  // poll ever (`cursor.runId === null`) was itself cursor-less, so its
  // response is already the complete run — adopt it and render. A run
  // change *mid-session* means this response was fetched under the old
  // run's stale cursor and is therefore partial (or empty): drop it without
  // rendering, reset the cursor, and ask the caller to re-poll immediately
  // (`refetchNow`) rather than waiting out a full `POLL_MS` showing nothing.
  function mergePoll(cursor, state) {
    if (state.run && state.run.id !== cursor.runId) {
      if (cursor.runId === null) {
        return { cursor: { since: state.events_seq, runId: state.run.id, events: state.events || [] }, render: true, refetchNow: false };
      }
      return { cursor: { since: null, runId: state.run.id, events: [] }, render: false, refetchNow: true };
    }
    var events = cursor.events.concat(state.events || []).slice(-EVENT_CAP);
    // An `events_seq` of 0 is a real cursor (the run's only event so far is
    // `run_start`) and must be honored — `||` would treat it as falsy and
    // keep re-requesting the whole run. But with no run at all (evidence
    // exists, nothing has started yet) there is nothing to be a cursor into:
    // keep `since` null so the first run is fetched in full, `seq` 0 included.
    var since = state.run && typeof state.events_seq === "number" ? state.events_seq : cursor.since;
    return { cursor: { since: since, runId: cursor.runId, events: events }, render: true, refetchNow: false };
  }

  function boot() {
    var blob = JSON.parse(document.getElementById("keel-data").textContent);
    // Static and watch exports render the embedded blob directly, whether
    // opened via `file://` or served over `http(s)://` (e.g. attached to a
    // ticket). Only `serve` mode polls — that's the only mode with a live
    // `./api/state` endpoint behind it.
    if (blob.mode !== "serve") {
      render(blob);
      if (blob.mode === "watch") setTimeout(function () { location.reload(); }, (blob.watch_interval_ms || 2000) + 250);
      return;
    }
    var cursor = { since: null, runId: null, events: [] }, failures = 0;
    function tick() {
      var url = "./api/state" + (cursor.since === null ? "" : "?since=" + cursor.since);
      var delay = POLL_MS;
      fetch(url, { cache: "no-store" })
        .then(function (r) { if (!r.ok) throw new Error("HTTP " + r.status); return r.json(); })
        .then(function (state) {
          var result = mergePoll(cursor, state);
          cursor = result.cursor;
          if (result.render) { state.events = cursor.events; render(state); }
          if (result.refetchNow) delay = 0;
          failures = 0;
          setStalled(false);
        })
        .catch(function () { failures += 1; if (failures >= 2) setStalled(true); })
        .then(function () { setTimeout(tick, delay); });
    }
    tick();
  }

  globalThis.KeelReport = { viewModel: viewModel, render: render, eventLine: eventLine, mergePoll: mergePoll };
  if (typeof document !== "undefined" && document.getElementById("keel-data")) boot();
})();
