//! Applyable policy diffs — "diffs as the lingua franca" (dx-spec §5).
//!
//! Every suggestion Keel makes — `keel init --diff` adds/removes, `doctor` fix
//! suggestions, and (future) `keel flows add` and `keel mcp propose_policy` —
//! is emitted as an *applyable* diff, because agents apply diffs reliably and
//! paraphrase prose unreliably. Callers describe a change as [`PolicyOp`]s
//! against the current `keel.toml` text; [`propose`] returns a [`Proposal`]:
//!
//! - `patch` — a unified diff (`a/keel.toml` → `b/keel.toml`, or `/dev/null`
//!   when the file does not exist yet) that `git apply` or `patch -p1` applies
//!   cleanly, `\ No newline at end of file` markers included.
//! - `changes` — structured `{path, before, after}` hunks for machines
//!   (comments are a text-level concern; they appear only in the patch).
//! - `new_text` — the full proposed file, for callers that write directly.
//!
//! Edits are surgical: they operate on the TOML *document* (`toml_edit`), not
//! a value round-trip, so user formatting and comments outside the touched
//! regions survive byte-for-byte. Everything here is a pure function of its
//! inputs — identical inputs yield byte-identical output (dx-spec §5).

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;

use serde::Serialize;
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value};

/// The policy file name used in patch headers (`a/keel.toml` → `b/keel.toml`).
const FILE_NAME: &str = "keel.toml";

/// Context lines around each hunk (git's default).
const CONTEXT: usize = 3;

/// A path into the policy document, segment by segment — e.g.
/// `["target", "api.example.com", "retry"]`. [`Display`](fmt::Display) renders
/// the TOML dotted-key form (`target."api.example.com".retry`), quoting
/// segments that are not bare keys, so paths stay unambiguous even when a
/// target name contains dots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyPath(Vec<String>);

impl PolicyPath {
    /// Build a path from segments.
    pub fn new<I, S>(segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(segments.into_iter().map(Into::into).collect())
    }

    /// The raw segments.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.0
    }
}

impl fmt::Display for PolicyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(&quote_segment(seg))?;
        }
        Ok(())
    }
}

/// Quote one key segment TOML-style when it is not a bare key.
fn quote_segment(seg: &str) -> String {
    let bare = !seg.is_empty()
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        seg.to_owned()
    } else {
        format!("\"{}\"", seg.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// One edit to the policy document. Within a proposal, [`Set`](PolicyOp::Set)
/// and [`Remove`](PolicyOp::Remove) apply first (in order) as document
/// surgery; every [`AppendBlock`](PolicyOp::AppendBlock) is then appended to
/// the rendered text (blank-line separated), so pre-rendered blocks keep their
/// comments verbatim.
#[derive(Debug, Clone)]
pub enum PolicyOp {
    /// Append a pre-rendered TOML block (evidence comments and all) at the end
    /// of the file. The block must parse in context; [`propose`] re-validates.
    AppendBlock {
        /// The block text, e.g. a whole `[target."…"]` section.
        text: String,
    },
    /// Remove the entry at `path` — a whole `[target."…"]` table or a single
    /// key. Removing a missing path is a no-op, so proposals stay idempotent.
    Remove {
        /// What to remove.
        path: PolicyPath,
    },
    /// Set the value at `path`, creating parent tables as needed (top-level
    /// groups as implicit tables, the level under them as `[dotted]` tables,
    /// anything deeper house-style inline: `retry = { … }`).
    Set {
        /// Where to write.
        path: PolicyPath,
        /// The value to write.
        value: Value,
    },
}

/// Why a proposal could not be built.
#[derive(Debug)]
pub enum DiffError {
    /// The current `keel.toml` text is not valid TOML.
    CurrentInvalid(String),
    /// A [`PolicyOp::Set`] path traverses something that is not a table.
    PathConflict {
        /// The offending path, TOML-dotted.
        path: String,
        /// What went wrong there.
        detail: String,
    },
    /// The proposed result does not parse (e.g. a malformed or colliding
    /// [`PolicyOp::AppendBlock`]).
    ResultInvalid(String),
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentInvalid(e) => write!(f, "keel.toml is not valid TOML: {e}"),
            Self::PathConflict { path, detail } => {
                write!(f, "cannot edit `{path}`: {detail}")
            }
            Self::ResultInvalid(e) => {
                write!(f, "the proposed keel.toml would not parse: {e}")
            }
        }
    }
}

impl std::error::Error for DiffError {}

/// One structured change: the TOML path plus the JSON value on each side
/// (`null` = absent). Emitted depth-first over sorted keys — deterministic.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeHunk {
    /// The value after the change (`null` when removed).
    pub after: Option<serde_json::Value>,
    /// The value before the change (`null` when added).
    pub before: Option<serde_json::Value>,
    /// TOML-dotted path, quoted where needed: `target."api.example.com".retry`.
    pub path: String,
}

/// The applyable result of [`propose`]: serialize it (or embed it in a larger
/// report) and both audiences are served — `patch` for `git apply`, `changes`
/// for structured consumption.
#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    /// Structured `{path, before, after}` hunks (see [`ChangeHunk`]).
    pub changes: Vec<ChangeHunk>,
    /// The full proposed file text, for callers that write directly. Not
    /// serialized — the patch is the wire form.
    #[serde(skip)]
    pub new_text: String,
    /// Unified diff `a/keel.toml` → `b/keel.toml` (`--- /dev/null` when the
    /// file does not exist yet). Empty when the ops change nothing.
    pub patch: String,
}

/// Build an applyable proposal: apply `ops` to `current` (the `keel.toml`
/// text; `None` = the file does not exist, which is distinct from
/// `Some("")` — it selects the `/dev/null` creation header) and return the
/// patch + structured changes. Untouched regions of `current` survive
/// byte-for-byte.
pub fn propose(current: Option<&str>, ops: &[PolicyOp]) -> Result<Proposal, DiffError> {
    let old_text = current.unwrap_or("");
    let mut doc: DocumentMut = old_text
        .parse()
        .map_err(|e: toml_edit::TomlError| DiffError::CurrentInvalid(e.to_string()))?;

    for op in ops {
        match op {
            PolicyOp::Set { path, value } => set_at(&mut doc, path, value.clone())?,
            PolicyOp::Remove { path } => remove_at(&mut doc, path),
            PolicyOp::AppendBlock { .. } => {}
        }
    }
    let mut new_text = doc.to_string();
    for op in ops {
        if let PolicyOp::AppendBlock { text } = op {
            append_block(&mut new_text, text);
        }
    }
    // Safety net: the proposed file must parse. A malformed AppendBlock (or one
    // colliding with an existing table) surfaces here, never at apply time.
    if let Err(e) = new_text.parse::<DocumentMut>() {
        return Err(DiffError::ResultInvalid(e.to_string()));
    }

    let patch = if new_text == old_text {
        String::new()
    } else {
        let old_label = if current.is_some() {
            format!("a/{FILE_NAME}")
        } else {
            "/dev/null".to_owned()
        };
        unified_diff(&old_label, &format!("b/{FILE_NAME}"), old_text, &new_text)
    };
    let before = policy_json(old_text).map_err(DiffError::CurrentInvalid)?;
    let after = policy_json(&new_text).map_err(DiffError::ResultInvalid)?;
    Ok(Proposal {
        changes: structural_changes(&before, &after),
        new_text,
        patch,
    })
}

/// Resolve a `serde_path_to_error`-style dotted path (e.g.
/// `target.api.example.com.retry.attempts`, where the target key itself
/// contains dots) against the document, greedily matching the longest key at
/// each level. Array indices (`on[1]`) are stripped. When the path descends
/// into a non-table value with segments left over, the path *to that value* is
/// returned — it is the entry to fix. Returns `None` when nothing matches.
#[must_use]
pub fn resolve_dotted_path(text: &str, dotted: &str) -> Option<PolicyPath> {
    let value: toml::Value = text.parse().ok()?;
    let segments: Vec<String> = dotted
        .split('.')
        .map(strip_index_suffix)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return None;
    }
    let mut resolved: Vec<String> = Vec::new();
    let mut current = &value;
    let mut rest = segments.as_slice();
    while !rest.is_empty() {
        let Some(table) = current.as_table() else {
            // Descended into a scalar/array with segments left: the value we
            // reached is the entry the caller should act on.
            break;
        };
        // Longest join of the remaining segments that names an actual key wins,
        // so `api.example.com` resolves as one segment.
        let matched = (1..=rest.len())
            .rev()
            .map(|n| rest[..n].join("."))
            .find(|candidate| table.contains_key(candidate))?;
        let consumed = matched.matches('.').count() + 1;
        current = &table[&matched];
        resolved.push(matched);
        rest = &rest[consumed..];
    }
    Some(PolicyPath(resolved))
}

/// Drop a trailing `[N]`… index suffix from one dotted-path segment.
fn strip_index_suffix(seg: &str) -> String {
    let mut out = seg;
    while let Some(open) = out.rfind('[') {
        if out.ends_with(']')
            && out[open + 1..out.len() - 1]
                .chars()
                .all(|c| c.is_ascii_digit())
        {
            out = &out[..open];
        } else {
            break;
        }
    }
    out.to_owned()
}

// ---- document surgery ----

/// Set `value` at `path`, creating parents as needed and preserving the decor
/// (spacing, trailing comment) of a value being overwritten.
fn set_at(doc: &mut DocumentMut, path: &PolicyPath, value: Value) -> Result<(), DiffError> {
    let Some((leaf, parents)) = path.segments().split_last() else {
        return Err(DiffError::PathConflict {
            path: String::new(),
            detail: "an empty path names nothing".to_owned(),
        });
    };
    let mut current: &mut dyn TableLike = doc.as_table_mut();
    let mut parent_is_standard = true;
    for (depth, seg) in parents.iter().enumerate() {
        if current.get(seg).is_none() {
            current.insert(seg, new_container(depth, parent_is_standard));
        }
        let item = current.get_mut(seg).expect("inserted above when missing");
        parent_is_standard = item.is_table();
        current = item
            .as_table_like_mut()
            .ok_or_else(|| DiffError::PathConflict {
                path: path.to_string(),
                detail: format!("`{seg}` already holds a plain value, not a table"),
            })?;
    }
    let existing_decor = current
        .get(leaf)
        .and_then(Item::as_value)
        .map(|v| v.decor().clone());
    let mut value = if parent_is_standard || existing_decor.is_some() {
        value.decorated(" ", "")
    } else {
        // A new last entry in an inline table: the closing-brace space moves
        // from the previous last value onto this one (`{ a = 1, b = 2 }`).
        let last_key = current.iter().last().map(|(k, _)| k.to_owned());
        if let Some(prev) = last_key
            .and_then(|k| current.get_mut(&k))
            .and_then(Item::as_value_mut)
            && prev
                .decor()
                .suffix()
                .and_then(toml_edit::RawString::as_str)
                .is_some_and(|s| !s.is_empty() && s.chars().all(char::is_whitespace))
        {
            prev.decor_mut().set_suffix("");
        }
        value.decorated(" ", " ")
    };
    if let Some(decor) = existing_decor {
        *value.decor_mut() = decor;
    }
    current.insert(leaf, Item::Value(value));
    Ok(())
}

/// A fresh intermediate container. Top-level groups (`target`, `flow`) become
/// implicit tables and the level under them explicit `[dotted]` tables — the
/// house style of generated files; anything deeper (or anything under an
/// inline table) stays inline (`retry = { … }`).
fn new_container(depth: usize, parent_is_standard: bool) -> Item {
    if parent_is_standard && depth <= 1 {
        let mut table = Table::new();
        table.set_implicit(depth == 0);
        Item::Table(table)
    } else {
        Item::Value(Value::InlineTable(InlineTable::new()).decorated(" ", ""))
    }
}

/// Remove the entry at `path`; a missing path is a no-op. When a whole
/// `[table]` block is removed, the part of its leading trivia that belongs to
/// the surrounding file (everything up to the last blank line — e.g. the
/// generated-file header comments above the first block) is re-attached to the
/// next block instead of vanishing with it.
fn remove_at(doc: &mut DocumentMut, path: &PolicyPath) {
    let Some((leaf, parents)) = path.segments().split_last() else {
        return;
    };
    let mut current: &mut dyn TableLike = doc.as_table_mut();
    for seg in parents {
        let Some(item) = current.get_mut(seg) else {
            return;
        };
        let Some(table) = item.as_table_like_mut() else {
            return;
        };
        current = table;
    }
    let salvage = match current.get(leaf) {
        None => return,
        Some(Item::Table(t)) => {
            let prefix = t
                .decor()
                .prefix()
                .and_then(toml_edit::RawString::as_str)
                .unwrap_or("");
            Some((detached_prefix(prefix), t.position()))
        }
        Some(_) => None,
    };
    current.remove(leaf);
    if let Some((detached, position)) = salvage
        && !detached.is_empty()
    {
        reattach_prefix(doc, position, &detached);
    }
}

/// The part of a removed table's leading trivia that belongs to the
/// surrounding file, not the block: everything up to and including the last
/// blank line. The trailing comment run (attached to the block) is dropped
/// with it; a whitespace-only remainder yields nothing, because the successor
/// keeps its own separator.
fn detached_prefix(prefix: &str) -> String {
    let detached = prefix.rfind("\n\n").map_or("", |i| &prefix[..i + 2]);
    if detached.trim().is_empty() {
        String::new()
    } else {
        detached.to_owned()
    }
}

/// Prepend `detached` to the leading trivia of the first `[table]` positioned
/// after `removed_position`; with no successor it becomes the document's
/// trailing content.
fn reattach_prefix(doc: &mut DocumentMut, removed_position: Option<usize>, detached: &str) {
    let successor = removed_position.and_then(|removed| {
        let mut best: Option<(usize, Vec<String>)> = None;
        collect_positioned_tables(doc.as_table(), &mut Vec::new(), &mut |pos, path| {
            if pos > removed && best.as_ref().is_none_or(|(b, _)| pos < *b) {
                best = Some((pos, path.to_vec()));
            }
        });
        best
    });
    if let Some((_, segments)) = successor {
        if let Some(table) = table_at_mut(doc, &segments) {
            let existing = table
                .decor()
                .prefix()
                .and_then(toml_edit::RawString::as_str)
                .unwrap_or("")
                .trim_start_matches('\n')
                .to_owned();
            table
                .decor_mut()
                .set_prefix(format!("{detached}{existing}"));
        }
    } else {
        let trailing = doc.trailing().as_str().unwrap_or("").to_owned();
        doc.set_trailing(format!("{trailing}{detached}"));
    }
}

/// Depth-first visit of every explicitly positioned `[table]` in the document.
fn collect_positioned_tables(
    table: &Table,
    path: &mut Vec<String>,
    visit: &mut dyn FnMut(usize, &[String]),
) {
    for (key, item) in table {
        if let Item::Table(t) = item {
            path.push(key.to_owned());
            if let Some(position) = t.position() {
                visit(position, path);
            }
            collect_positioned_tables(t, path, visit);
            path.pop();
        }
    }
}

/// Navigate to the `[table]` at `segments` (standard tables only).
fn table_at_mut<'a>(doc: &'a mut DocumentMut, segments: &[String]) -> Option<&'a mut Table> {
    let mut table: &mut Table = doc.as_table_mut();
    for seg in segments {
        match table.get_mut(seg) {
            Some(Item::Table(t)) => table = t,
            _ => return None,
        }
    }
    Some(table)
}

/// Append a pre-rendered block with exactly one blank line separating it from
/// existing content, normalizing to a single trailing newline.
fn append_block(out: &mut String, block: &str) {
    let block = block.trim_end_matches('\n');
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
    out.push_str(block);
    out.push('\n');
}

// ---- structured changes ----

/// Parse policy text to a JSON value (comments and formatting drop away — this
/// is the semantic view the `changes` hunks compare).
fn policy_json(text: &str) -> Result<serde_json::Value, String> {
    let value: toml::Value = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    serde_json::to_value(value).map_err(|e| e.to_string())
}

/// Structurally compare two policy documents into `{path, before, after}`
/// hunks. Tables present on both sides recurse; a subtree appearing or
/// vanishing above the target-name level (depth < 2) is broken into per-child
/// hunks so adds/removes stay `[target."…"]`-block granular.
fn structural_changes(before: &serde_json::Value, after: &serde_json::Value) -> Vec<ChangeHunk> {
    let mut out = Vec::new();
    walk_changes(&mut Vec::new(), Some(before), Some(after), &mut out);
    out
}

fn walk_changes(
    path: &mut Vec<String>,
    before: Option<&serde_json::Value>,
    after: Option<&serde_json::Value>,
    out: &mut Vec<ChangeHunk>,
) {
    if before == after {
        return;
    }
    let recurse = match (before, after) {
        (Some(b), Some(a)) => b.is_object() && a.is_object(),
        (Some(v), None) | (None, Some(v)) => v.is_object() && path.len() < 2,
        (None, None) => return,
    };
    if recurse {
        let empty = serde_json::Map::new();
        let b = before
            .and_then(serde_json::Value::as_object)
            .unwrap_or(&empty);
        let a = after
            .and_then(serde_json::Value::as_object)
            .unwrap_or(&empty);
        let keys: BTreeSet<&String> = b.keys().chain(a.keys()).collect();
        for key in keys {
            path.push(key.clone());
            walk_changes(path, b.get(key), a.get(key), out);
            path.pop();
        }
        return;
    }
    out.push(ChangeHunk {
        after: after.cloned(),
        before: before.cloned(),
        path: PolicyPath::new(path.iter().cloned()).to_string(),
    });
}

// ---- unified diff ----

/// One line-level edit; indices point into the old/new line vectors. An equal
/// line is rendered from the old side, so it only carries its old index.
#[derive(Debug, Clone, Copy)]
enum Edit {
    /// The same line on both sides (old index).
    Equal(usize),
    /// A line only in the old text.
    Del(usize),
    /// A line only in the new text.
    Ins(usize),
}

/// Render a unified diff between two texts: git-compatible headers, three
/// context lines, `\ No newline at end of file` markers. Lines are compared
/// *with* their terminators, so a missing final newline diffs correctly.
/// Deterministic — a pure function of its inputs.
#[must_use]
pub fn unified_diff(old_label: &str, new_label: &str, old: &str, new: &str) -> String {
    let old_lines = split_keep_newline(old);
    let new_lines = split_keep_newline(new);
    let edits = line_edits(&old_lines, &new_lines);
    let mut out = format!("--- {old_label}\n+++ {new_label}\n");
    for range in hunk_ranges(&edits) {
        let (old_pos, new_pos) = cursor_at(&edits, range.start);
        render_hunk(
            &mut out,
            &edits[range],
            old_pos,
            new_pos,
            &old_lines,
            &new_lines,
        );
    }
    out
}

/// Split text into lines that *keep* their `\n`, so a terminator-less final
/// line compares as different from its terminated twin.
fn split_keep_newline(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(i) = rest.find('\n') {
            lines.push(&rest[..=i]);
            rest = &rest[i + 1..];
        } else {
            lines.push(rest);
            rest = "";
        }
    }
    lines
}

/// The line-level edit script: common prefix/suffix trimmed, then an LCS walk
/// over the middle. Policy files are small; O(n·m) is comfortably fine.
fn line_edits(old: &[&str], new: &[&str]) -> Vec<Edit> {
    let prefix = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let rest_old = &old[prefix..];
    let rest_new = &new[prefix..];
    let suffix = rest_old
        .iter()
        .rev()
        .zip(rest_new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &rest_old[..rest_old.len() - suffix];
    let b = &rest_new[..rest_new.len() - suffix];

    // lcs[i][j] = length of the LCS of a[i..] and b[j..].
    let mut lcs = vec![vec![0_usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut edits: Vec<Edit> = (0..prefix).map(Edit::Equal).collect();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            edits.push(Edit::Equal(prefix + i));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            edits.push(Edit::Del(prefix + i));
            i += 1;
        } else {
            edits.push(Edit::Ins(prefix + j));
            j += 1;
        }
    }
    while i < a.len() {
        edits.push(Edit::Del(prefix + i));
        i += 1;
    }
    while j < b.len() {
        edits.push(Edit::Ins(prefix + j));
        j += 1;
    }
    for k in 0..suffix {
        edits.push(Edit::Equal(old.len() - suffix + k));
    }
    edits
}

/// Group changed edits into hunk ranges, each padded with [`CONTEXT`] edits and
/// merged when their context regions touch.
fn hunk_ranges(edits: &[Edit]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        if matches!(edit, Edit::Equal(..)) {
            continue;
        }
        let start = index.saturating_sub(CONTEXT);
        let end = (index + CONTEXT + 1).min(edits.len());
        match ranges.last_mut() {
            Some(last) if start <= last.end => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
}

/// The (old, new) line counts consumed by `edits[..upto]` — the hunk's cursor.
fn cursor_at(edits: &[Edit], upto: usize) -> (usize, usize) {
    let mut old = 0;
    let mut new = 0;
    for edit in &edits[..upto] {
        match edit {
            Edit::Equal(..) => {
                old += 1;
                new += 1;
            }
            Edit::Del(_) => old += 1,
            Edit::Ins(_) => new += 1,
        }
    }
    (old, new)
}

/// Render one `@@ -l,c +l,c @@` hunk. A zero-count side reports the line
/// *before* the change (git's convention, `-0,0` for creation).
fn render_hunk(
    out: &mut String,
    edits: &[Edit],
    old_pos: usize,
    new_pos: usize,
    old: &[&str],
    new: &[&str],
) {
    let old_count = edits
        .iter()
        .filter(|e| matches!(e, Edit::Equal(..) | Edit::Del(_)))
        .count();
    let new_count = edits
        .iter()
        .filter(|e| matches!(e, Edit::Equal(..) | Edit::Ins(_)))
        .count();
    let old_display = if old_count == 0 { old_pos } else { old_pos + 1 };
    let new_display = if new_count == 0 { new_pos } else { new_pos + 1 };
    let header = format!("@@ -{old_display},{old_count} +{new_display},{new_count} @@\n");
    out.push_str(&header);
    for edit in edits {
        match *edit {
            Edit::Equal(i) => push_line(out, ' ', old[i]),
            Edit::Del(i) => push_line(out, '-', old[i]),
            Edit::Ins(j) => push_line(out, '+', new[j]),
        }
    }
}

/// Emit one patch body line; a terminator-less line gets the
/// `\ No newline at end of file` marker git and patch expect.
fn push_line(out: &mut String, tag: char, line: &str) {
    out.push(tag);
    if let Some(body) = line.strip_suffix('\n') {
        out.push_str(body);
        out.push('\n');
    } else {
        out.push_str(line);
        out.push_str("\n\\ No newline at end of file\n");
    }
}

#[cfg(test)]
pub(crate) use tests::apply_unified;

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only unified-diff applier: replays `patch` against `old` exactly
    /// the way `git apply`/`patch -p1` would, so the property "the emitted
    /// patch applies cleanly and reproduces `new_text`" is checked hermetically.
    pub(crate) fn apply_unified(old: &str, patch: &str) -> Result<String, String> {
        let old_lines = split_keep_newline(old);
        let mut out = String::new();
        let mut cursor = 0_usize;
        let mut lines = patch.lines().peekable();
        while lines
            .peek()
            .is_some_and(|l| l.starts_with("--- ") || l.starts_with("+++ "))
        {
            lines.next();
        }
        while let Some(header) = lines.next() {
            let (old_start, old_count) = parse_hunk_side(header, '-')?;
            let (_, new_count) = parse_hunk_side(header, '+')?;
            let hunk_old_index = if old_count == 0 {
                old_start
            } else {
                old_start - 1
            };
            while cursor < hunk_old_index {
                out.push_str(old_lines[cursor]);
                cursor += 1;
            }
            let (mut consumed, mut produced) = (0, 0);
            while consumed < old_count || produced < new_count {
                let line = lines.next().ok_or("truncated hunk")?;
                let body = &line[1..];
                let no_newline_next = lines.peek() == Some(&"\\ No newline at end of file");
                let text = if no_newline_next {
                    lines.next();
                    body.to_owned()
                } else {
                    format!("{body}\n")
                };
                match line.as_bytes().first() {
                    Some(b' ') => {
                        if old_lines[cursor] != text {
                            return Err(format!("context mismatch at old line {cursor}"));
                        }
                        out.push_str(&text);
                        cursor += 1;
                        consumed += 1;
                        produced += 1;
                    }
                    Some(b'-') => {
                        if old_lines[cursor] != text {
                            return Err(format!("removal mismatch at old line {cursor}"));
                        }
                        cursor += 1;
                        consumed += 1;
                    }
                    Some(b'+') => {
                        out.push_str(&text);
                        produced += 1;
                    }
                    _ => return Err(format!("unexpected patch line: {line}")),
                }
            }
        }
        while cursor < old_lines.len() {
            out.push_str(old_lines[cursor]);
            cursor += 1;
        }
        Ok(out)
    }

    /// Parse one side (`-` or `+`) of an `@@ -l,c +l,c @@` header.
    fn parse_hunk_side(header: &str, sign: char) -> Result<(usize, usize), String> {
        let start = header
            .find(sign)
            .ok_or_else(|| format!("bad hunk header: {header}"))?;
        let rest = &header[start + 1..];
        let end = rest
            .find(' ')
            .ok_or_else(|| format!("bad hunk header: {header}"))?;
        let (line, count) = rest[..end]
            .split_once(',')
            .ok_or_else(|| format!("bad hunk header: {header}"))?;
        Ok((
            line.parse().map_err(|e| format!("bad line number: {e}"))?,
            count.parse().map_err(|e| format!("bad count: {e}"))?,
        ))
    }

    // ---- unified_diff ----

    #[test]
    fn modify_one_line_yields_one_hunk_with_three_context_lines() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nc\nD\ne\nf\ng\n";
        let patch = unified_diff("a/keel.toml", "b/keel.toml", old, new);
        assert_eq!(
            patch,
            "--- a/keel.toml\n+++ b/keel.toml\n@@ -1,7 +1,7 @@\n a\n b\n c\n-d\n+D\n e\n f\n g\n"
        );
        assert_eq!(apply_unified(old, &patch).unwrap(), new);
    }

    #[test]
    fn creation_from_absent_file_counts_from_zero() {
        let patch = unified_diff("/dev/null", "b/keel.toml", "", "x = 1\ny = 2\n");
        assert_eq!(
            patch,
            "--- /dev/null\n+++ b/keel.toml\n@@ -0,0 +1,2 @@\n+x = 1\n+y = 2\n"
        );
        assert_eq!(apply_unified("", &patch).unwrap(), "x = 1\ny = 2\n");
    }

    #[test]
    fn missing_final_newline_gets_the_marker_on_the_right_side() {
        let old = "a\nb";
        let new = "a\nb\nc\n";
        let patch = unified_diff("a/keel.toml", "b/keel.toml", old, new);
        assert_eq!(
            patch,
            "--- a/keel.toml\n+++ b/keel.toml\n@@ -1,2 +1,3 @@\n a\n-b\n\\ No newline at end of file\n+b\n+c\n"
        );
        assert_eq!(apply_unified(old, &patch).unwrap(), new);
    }

    #[test]
    fn distant_changes_land_in_separate_hunks() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n";
        let new = "one\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\nfifteen\n";
        let patch = unified_diff("a/keel.toml", "b/keel.toml", old, new);
        assert_eq!(patch.matches("@@").count(), 4, "two hunks: {patch}");
        assert_eq!(apply_unified(old, &patch).unwrap(), new);
    }

    // ---- propose: surgical edits ----

    const TUNED: &str = "\
# hand-tuned; keep me

[target.\"api.example.com\"]        # seen in: app.py:4
timeout = \"30s\"                      # tuned by us
retry   = { attempts = 3 }

[target.\"api.other.example\"]
timeout = \"10s\"
";

    #[test]
    fn set_overwrites_in_place_preserving_comments_and_untouched_lines() {
        let ops = [PolicyOp::Set {
            path: PolicyPath::new(["target", "api.example.com", "timeout"]),
            value: Value::from("2m"),
        }];
        let p = propose(Some(TUNED), &ops).unwrap();
        // Only the timeout value changed; its trailing comment and every other
        // byte of the file survive.
        assert_eq!(p.new_text, TUNED.replace("\"30s\"", "\"2m\""));
        assert_eq!(apply_unified(TUNED, &p.patch).unwrap(), p.new_text);
        assert_eq!(p.changes.len(), 1);
        assert_eq!(p.changes[0].path, "target.\"api.example.com\".timeout");
        assert_eq!(p.changes[0].before, Some(serde_json::json!("30s")));
        assert_eq!(p.changes[0].after, Some(serde_json::json!("2m")));
    }

    #[test]
    fn set_creates_missing_tables_in_house_style() {
        let ops = [PolicyOp::Set {
            path: PolicyPath::new(["target", "api.x", "retry", "attempts"]),
            value: Value::from(5_i64),
        }];
        let p = propose(Some(""), &ops).unwrap();
        assert_eq!(p.new_text, "[target.\"api.x\"]\nretry = { attempts = 5 }\n");
        assert_eq!(apply_unified("", &p.patch).unwrap(), p.new_text);
        // Existing file: the new key lands inside the existing inline table.
        let ops = [PolicyOp::Set {
            path: PolicyPath::new(["target", "api.example.com", "retry", "schedule"]),
            value: Value::from("exp(1s, x2, max 30s)"),
        }];
        let p = propose(Some(TUNED), &ops).unwrap();
        assert!(
            p.new_text
                .contains("retry   = { attempts = 3, schedule = \"exp(1s, x2, max 30s)\" }"),
            "got: {}",
            p.new_text
        );
        assert_eq!(apply_unified(TUNED, &p.patch).unwrap(), p.new_text);
    }

    #[test]
    fn set_through_a_scalar_is_a_path_conflict() {
        let ops = [PolicyOp::Set {
            path: PolicyPath::new(["target", "api.example.com", "timeout", "nested"]),
            value: Value::from(1_i64),
        }];
        let err = propose(Some(TUNED), &ops).unwrap_err();
        assert!(matches!(err, DiffError::PathConflict { .. }), "{err}");
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn remove_drops_a_whole_block_but_keeps_file_header_comments() {
        let generated = "\
# Generated by keel init from 2 static scans + 0 observed runs
# Every entry below was found in YOUR code. Delete anything; defaults still apply.

[target.\"a.example\"]               # seen in: app.py:4
timeout = \"30s\"

[target.\"b.example\"]
timeout = \"10s\"
";
        // Removing the FIRST block re-attaches the header comments to the next.
        let ops = [PolicyOp::Remove {
            path: PolicyPath::new(["target", "a.example"]),
        }];
        let p = propose(Some(generated), &ops).unwrap();
        assert_eq!(
            p.new_text,
            "\
# Generated by keel init from 2 static scans + 0 observed runs
# Every entry below was found in YOUR code. Delete anything; defaults still apply.

[target.\"b.example\"]
timeout = \"10s\"
"
        );
        assert_eq!(apply_unified(generated, &p.patch).unwrap(), p.new_text);
        // Removing the LAST block leaves no dangling blank line.
        let ops = [PolicyOp::Remove {
            path: PolicyPath::new(["target", "b.example"]),
        }];
        let p = propose(Some(generated), &ops).unwrap();
        assert!(
            p.new_text.ends_with("timeout = \"30s\"\n"),
            "{}",
            p.new_text
        );
        assert!(!p.new_text.contains("b.example"));
        assert_eq!(apply_unified(generated, &p.patch).unwrap(), p.new_text);
        assert_eq!(p.changes.len(), 1);
        assert_eq!(p.changes[0].path, "target.\"b.example\"");
        assert!(p.changes[0].after.is_none());
    }

    #[test]
    fn remove_of_a_missing_path_is_a_noop() {
        let ops = [PolicyOp::Remove {
            path: PolicyPath::new(["target", "not.there"]),
        }];
        let p = propose(Some(TUNED), &ops).unwrap();
        assert_eq!(p.new_text, TUNED);
        assert!(p.patch.is_empty());
        assert!(p.changes.is_empty());
    }

    #[test]
    fn remove_inside_an_inline_table_drops_just_that_key() {
        let ops = [PolicyOp::Remove {
            path: PolicyPath::new(["target", "api.example.com", "retry", "attempts"]),
        }];
        let p = propose(Some(TUNED), &ops).unwrap();
        assert!(p.new_text.contains("retry   = {  }") || p.new_text.contains("retry   = {}"));
        assert_eq!(apply_unified(TUNED, &p.patch).unwrap(), p.new_text);
    }

    #[test]
    fn append_block_separates_with_exactly_one_blank_line() {
        let block = "[target.\"api.new.example\"]\ntimeout = \"30s\"\n";
        let p = propose(
            Some(TUNED),
            &[PolicyOp::AppendBlock {
                text: block.to_owned(),
            }],
        )
        .unwrap();
        assert!(
            p.new_text.ends_with(
                "timeout = \"10s\"\n\n[target.\"api.new.example\"]\ntimeout = \"30s\"\n"
            ),
            "{}",
            p.new_text
        );
        assert_eq!(apply_unified(TUNED, &p.patch).unwrap(), p.new_text);
        assert_eq!(p.changes.len(), 1);
        assert_eq!(p.changes[0].path, "target.\"api.new.example\"");
        assert!(p.changes[0].before.is_none());
    }

    #[test]
    fn append_to_a_file_without_trailing_newline_still_applies() {
        let old = "[target.\"api.example.com\"]\ntimeout = \"30s\""; // no final newline
        let block = "[target.\"api.new.example\"]\ntimeout = \"5s\"\n";
        let p = propose(
            Some(old),
            &[PolicyOp::AppendBlock {
                text: block.to_owned(),
            }],
        )
        .unwrap();
        assert!(p.patch.contains("\\ No newline at end of file"));
        assert_eq!(apply_unified(old, &p.patch).unwrap(), p.new_text);
        assert!(p.new_text.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn garbage_append_block_is_rejected_before_it_can_ship() {
        let err = propose(
            Some(TUNED),
            &[PolicyOp::AppendBlock {
                text: "not [valid toml".to_owned(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, DiffError::ResultInvalid(_)), "{err}");
        // A block colliding with an existing table is equally rejected.
        let err = propose(
            Some(TUNED),
            &[PolicyOp::AppendBlock {
                text: "[target.\"api.example.com\"]\ntimeout = \"1s\"\n".to_owned(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, DiffError::ResultInvalid(_)), "{err}");
    }

    #[test]
    fn invalid_current_text_is_reported_as_such() {
        let err = propose(Some("not [valid"), &[]).unwrap_err();
        assert!(matches!(err, DiffError::CurrentInvalid(_)), "{err}");
    }

    #[test]
    fn absent_file_proposes_a_dev_null_creation_patch() {
        let content = "# header\n\n[target.\"a.example\"]\ntimeout = \"30s\"\n";
        let p = propose(
            None,
            &[PolicyOp::AppendBlock {
                text: content.to_owned(),
            }],
        )
        .unwrap();
        assert_eq!(p.new_text, content);
        assert!(
            p.patch
                .starts_with("--- /dev/null\n+++ b/keel.toml\n@@ -0,0 +1,4 @@\n")
        );
        assert_eq!(apply_unified("", &p.patch).unwrap(), content);
        // Creation hunks stay target-block granular (depth rule).
        assert_eq!(p.changes.len(), 1);
        assert_eq!(p.changes[0].path, "target.\"a.example\"");
    }

    #[test]
    fn proposal_json_serializes_changes_and_patch_only() {
        let p = propose(
            Some(TUNED),
            &[PolicyOp::Remove {
                path: PolicyPath::new(["target", "api.other.example"]),
            }],
        )
        .unwrap();
        let json = crate::render::to_json(&p);
        assert!(json.get("changes").is_some());
        assert!(json.get("patch").is_some());
        assert!(
            json.get("new_text").is_none(),
            "new_text is not wire format"
        );
    }

    #[test]
    fn applying_the_patch_yields_a_file_that_parses_to_the_proposed_policy() {
        // The property from the task brief, over a mixed op set.
        let ops = [
            PolicyOp::Remove {
                path: PolicyPath::new(["target", "api.other.example"]),
            },
            PolicyOp::Set {
                path: PolicyPath::new(["target", "api.example.com", "timeout"]),
                value: Value::from("45s"),
            },
            PolicyOp::AppendBlock {
                text: "[target.\"api.new.example\"]\ntimeout = \"5s\"\n".to_owned(),
            },
        ];
        let p = propose(Some(TUNED), &ops).unwrap();
        let applied = apply_unified(TUNED, &p.patch).unwrap();
        assert_eq!(applied, p.new_text);
        let applied_policy: toml::Value = applied.parse().unwrap();
        let proposed_policy: toml::Value = p.new_text.parse().unwrap();
        assert_eq!(applied_policy, proposed_policy);
        assert!(applied_policy["target"].get("api.other.example").is_none());
        assert_eq!(
            applied_policy["target"]["api.example.com"]["timeout"].as_str(),
            Some("45s")
        );
    }

    // ---- paths ----

    #[test]
    fn policy_path_display_quotes_only_non_bare_segments() {
        let path = PolicyPath::new(["target", "api.example.com", "retry"]);
        assert_eq!(path.to_string(), "target.\"api.example.com\".retry");
        assert_eq!(
            PolicyPath::new(["flow", "nightly-etl"]).to_string(),
            "flow.nightly-etl"
        );
    }

    #[test]
    fn resolve_dotted_path_matches_the_longest_key_greedily() {
        let text = "[target.\"api.example.com\"]\nretry = { attempts = 0, on = [\"5xx\"] }\n";
        let resolved = resolve_dotted_path(text, "target.api.example.com.retry.attempts").unwrap();
        assert_eq!(
            resolved.segments(),
            ["target", "api.example.com", "retry", "attempts"]
        );
        // Array indices are stripped; the path stops at the array's value.
        let resolved = resolve_dotted_path(text, "target.api.example.com.retry.on[1]").unwrap();
        assert_eq!(
            resolved.segments(),
            ["target", "api.example.com", "retry", "on"]
        );
        // Segments beyond a scalar resolve to the scalar (the entry to fix).
        let resolved = resolve_dotted_path(text, "target.api.example.com.retry.attempts.deep");
        assert_eq!(
            resolved.unwrap().segments(),
            ["target", "api.example.com", "retry", "attempts"]
        );
        assert!(resolve_dotted_path(text, "target.nope.retry").is_none());
    }
}
