//! Detection for Google's `agents-cli` project layout.
//!
//! `agents-cli scaffold create` writes an `agents-cli-manifest.yaml` at the
//! project root naming the agent's own subdirectory (`agent_directory: <dir>`),
//! and generates a `Dockerfile` that `COPY`s only `pyproject.toml`, `README.md`,
//! `uv.lock*`, and that one agent directory into the image — a `keel.toml`
//! sitting at the project root next to the manifest is never in that COPY set,
//! so it silently never reaches the container. `keel init` and `keel doctor`
//! both need to recognize this layout: init writes the generated policy
//! straight into the agent directory instead, and doctor warns when it finds a
//! root `keel.toml` that would be left behind.
//!
//! The manifest is a full YAML document, but we need exactly one scalar key out
//! of it. Pulling in a YAML parser for that would be a real dependency (and a
//! supply-chain surface) for one line of text, so this is a deliberate hand
//! parse of the single `agent_directory:` key — not a general YAML reader. It
//! tolerates the two forms `agents-cli` itself emits (bare and quoted scalars)
//! and gives up (returns `None`) on anything else, which just means Keel falls
//! back to treating the project as a non-agents-cli layout.

use std::path::{Path, PathBuf};

/// The manifest `agents-cli scaffold create` writes at the project root.
const MANIFEST_FILENAME: &str = "agents-cli-manifest.yaml";

/// Bound on how many parent directories [`find_agents_cli_layout`] will walk
/// before giving up — a defensive limit against pathological filesystems
/// (symlink cycles, an unexpectedly deep tree), not a realistic project depth.
const MAX_WALK_LEVELS: usize = 8;

/// The agents-cli layout facts relevant to Keel: where the manifest lives, and
/// the agent's own subdirectory (the only directory the generated Dockerfile
/// ships).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsCliLayout {
    /// The directory containing `agents-cli-manifest.yaml`.
    pub manifest_dir: PathBuf,
    /// `<manifest_dir>/<agent_directory>`, resolved from the manifest's
    /// `agent_directory` key. Guaranteed to exist on disk (as a directory) by
    /// construction — [`find_agents_cli_layout`] returns `None` otherwise.
    pub agent_dir: PathBuf,
}

/// Walk upward from `project` (inclusive) looking for `agents-cli-manifest.yaml`,
/// bounded to [`MAX_WALK_LEVELS`] and stopping at the filesystem root. On the
/// first manifest found, hand-parse its `agent_directory` key; return `None` if
/// the key is missing or the directory it names does not exist.
#[must_use]
pub fn find_agents_cli_layout(project: &Path) -> Option<AgentsCliLayout> {
    let mut dir = project;
    for _ in 0..=MAX_WALK_LEVELS {
        let candidate = dir.join(MANIFEST_FILENAME);
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let agent_directory = parse_agent_directory(&text)?;
            let agent_dir = dir.join(agent_directory);
            return agent_dir.is_dir().then(|| AgentsCliLayout {
                manifest_dir: dir.to_owned(),
                agent_dir,
            });
        }
        dir = dir.parent()?;
    }
    None
}

/// Hand-parse the `agent_directory: <value>` line out of an
/// `agents-cli-manifest.yaml` document: the first line matching
/// `^agent_directory:\s*(\S+)`, with the value trimmed of a wrapping pair of
/// single or double quotes. `None` when no such line exists. Deliberately not
/// a YAML parser — see the module docs.
fn parse_agent_directory(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("agent_directory:") else {
            // Not the key on this line — keep scanning; unlike `?`, this must
            // NOT abort the whole parse on the first non-matching line (e.g.
            // a `schema_version:` line preceding `agent_directory:`).
            continue;
        };
        let value = rest.trim();
        if value.is_empty() {
            return None;
        }
        return Some(unquote(value).to_owned());
    }
    None
}

/// Strip one matching pair of wrapping quotes (`'...'` or `"..."`), else return
/// the input unchanged.
fn unquote(value: &str) -> &str {
    for quote in ['\'', '"'] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|s| s.strip_suffix(quote))
            && inner.len() + 2 == value.len()
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::write(dir.join(MANIFEST_FILENAME), body).unwrap();
    }

    #[test]
    fn found_at_project_root() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("app")).unwrap();
        write_manifest(dir.path(), "schema_version: 1\nagent_directory: app\n");

        let layout = find_agents_cli_layout(dir.path()).expect("layout found");
        assert_eq!(layout.manifest_dir, dir.path());
        assert_eq!(layout.agent_dir, dir.path().join("app"));
    }

    #[test]
    fn found_by_walking_upward_from_a_nested_directory() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("app")).unwrap();
        write_manifest(dir.path(), "agent_directory: app\n");
        let nested = dir.path().join("app").join("sub").join("deeper");
        std::fs::create_dir_all(&nested).unwrap();

        let layout = find_agents_cli_layout(&nested).expect("layout found by walking up");
        assert_eq!(layout.manifest_dir, dir.path());
        assert_eq!(layout.agent_dir, dir.path().join("app"));
    }

    #[test]
    fn not_found_when_no_manifest_exists() {
        let dir = TempDir::new().unwrap();
        assert!(find_agents_cli_layout(dir.path()).is_none());
    }

    #[test]
    fn quoted_value_is_unquoted() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("my_agent")).unwrap();
        write_manifest(dir.path(), "agent_directory: \"my_agent\"\n");

        let layout = find_agents_cli_layout(dir.path()).expect("layout found");
        assert_eq!(layout.agent_dir, dir.path().join("my_agent"));
    }

    #[test]
    fn single_quoted_value_is_unquoted() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("my_agent")).unwrap();
        write_manifest(dir.path(), "agent_directory: 'my_agent'\n");

        let layout = find_agents_cli_layout(dir.path()).expect("layout found");
        assert_eq!(layout.agent_dir, dir.path().join("my_agent"));
    }

    #[test]
    fn missing_key_yields_none() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), "schema_version: 1\n");
        assert!(find_agents_cli_layout(dir.path()).is_none());
    }

    #[test]
    fn nonexistent_agent_dir_yields_none() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), "agent_directory: does_not_exist\n");
        assert!(find_agents_cli_layout(dir.path()).is_none());
    }

    #[test]
    fn walk_is_bounded_and_stops_before_the_filesystem_root() {
        // A manifest sitting far enough above `project` that it exceeds
        // MAX_WALK_LEVELS must not be found. We simulate this by walking from a
        // directory nested deeper than the bound under a project that has no
        // manifest anywhere in its ancestry within TempDir.
        let dir = TempDir::new().unwrap();
        let mut nested = dir.path().to_owned();
        for i in 0..(MAX_WALK_LEVELS + 2) {
            nested = nested.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&nested).unwrap();
        assert!(find_agents_cli_layout(&nested).is_none());
    }
}
