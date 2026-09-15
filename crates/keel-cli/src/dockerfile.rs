//! Does `keel.toml` reach the container image? A static read of the root
//! build files' `COPY`/`ADD` directives (WS3, field report 2026-09-15 F8):
//! the policy is a file, files get left out of images, and a green
//! `keel doctor` on the checkout said nothing about the container that
//! actually failed. This is a lead, not a build: variables and unusual
//! source shapes are reported as indeterminate, never guessed.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    Reached,
    NotReached,
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildFile {
    /// Project-relative file name (root files only, so no directories).
    pub file: String,
    pub reach: Reach,
    /// The directive that decided `Indeterminate`, for the finding text.
    pub directive: Option<String>,
    /// For `Reached`: whether a reaching directive sits in the LAST `FROM`
    /// stage (a builder-only copy does not ship).
    pub reached_in_final_stage: bool,
}

const FIXED_NAMES: &[&str] = &["Dockerfile", "Containerfile"];

fn is_build_file_name(name: &str) -> bool {
    FIXED_NAMES.contains(&name)
        || name.starts_with("Dockerfile.")
        || name.starts_with("Containerfile.")
        || name.ends_with(".Dockerfile")
        || name.ends_with(".Containerfile")
}

/// Root-level build files, sorted by name. Root only on purpose: a nested
/// Dockerfile has a different build context and `keel.toml` may legitimately
/// be absent from it.
#[must_use]
pub fn find_build_files(project: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(project) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_build_file_name)
        })
        .collect();
    out.sort();
    out
}

/// Every root build file's verdict, sorted by file name.
#[must_use]
pub fn analyze(project: &Path) -> Vec<BuildFile> {
    let ignore = std::fs::read_to_string(project.join(".dockerignore")).unwrap_or_default();
    find_build_files(project)
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            let name = p.file_name()?.to_str()?.to_owned();
            Some(analyze_text(&name, &text, &ignore))
        })
        .collect()
}

/// Join backslash continuations, drop comments and blank lines.
///
/// A comment line is dropped wherever it appears — including in the MIDDLE of
/// a continuation, which is what Docker does (comment lines are removed before
/// continuations are joined, and a trailing `\` on a comment line does not
/// continue anything). Concatenating one instead would fold its tokens into the
/// surrounding `COPY`, so a commented-out `# keel.toml \` between two real
/// source lines would read as a source and silently suppress the warning.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(stripped) = line.strip_suffix('\\') {
            cur.push_str(stripped);
            cur.push(' ');
            continue;
        }
        cur.push_str(line);
        let joined = cur.trim().to_owned();
        cur.clear();
        if !joined.is_empty() {
            out.push(joined);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_owned());
    }
    out
}

/// `Some(sources)` for a COPY/ADD from the build context; `None` for any
/// other instruction or a `--from=` copy (which never reads the context).
fn copy_sources(line: &str) -> Option<Vec<String>> {
    let mut parts = line.split_whitespace();
    let instr = parts.next()?;
    if !instr.eq_ignore_ascii_case("COPY") && !instr.eq_ignore_ascii_case("ADD") {
        return None;
    }
    let rest: Vec<&str> = parts.collect();
    let mut args: Vec<&str> = Vec::new();
    for a in rest {
        if a.starts_with("--") {
            if a.starts_with("--from=") || a == "--from" {
                return None;
            }
            continue;
        }
        args.push(a);
    }
    let joined = args.join(" ");
    let items: Vec<String> = if joined.starts_with('[') {
        // JSON array form: ["src", "src2", "dest"]
        joined
            .trim_matches(|c| c == '[' || c == ']')
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        args.iter().map(|s| (*s).to_owned()).collect()
    };
    if items.len() < 2 {
        return None;
    }
    Some(items[..items.len() - 1].to_vec())
}

/// Minimal `*`/`?` glob over one path segment (no `/` crossing needed: we
/// only ever match the literal `keel.toml`).
fn glob_matches(pattern: &str, name: &str) -> bool {
    fn rec(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => rec(&p[1..], n) || (!n.is_empty() && rec(p, &n[1..])),
            (Some(b'?'), Some(_)) => rec(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => rec(&p[1..], &n[1..]),
            _ => false,
        }
    }
    rec(pattern.as_bytes(), name.as_bytes())
}

fn normalize(src: &str) -> String {
    let s = src.trim().trim_start_matches("./");
    s.trim_end_matches('/').to_owned()
}

/// Whether `.dockerignore` text excludes `keel.toml` from a context copy.
/// Docker semantics: last matching rule wins; `!` re-includes.
fn dockerignore_excludes(ignore: &str) -> bool {
    let mut excluded = false;
    for raw in ignore.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negate, pat) = match line.strip_prefix('!') {
            Some(p) => (true, p.trim()),
            None => (false, line),
        };
        let pat = normalize(pat);
        let pat = pat.strip_prefix("**/").unwrap_or(&pat);
        if pat == "**" || glob_matches(pat, "keel.toml") {
            excluded = !negate;
        }
    }
    excluded
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceReach {
    Explicit,
    ViaContext,
    No,
    Variable,
}

fn source_reach(src: &str) -> SourceReach {
    if src.contains('$') {
        return SourceReach::Variable;
    }
    let n = normalize(src);
    if n == "keel.toml" {
        return SourceReach::Explicit;
    }
    if n.is_empty() || n == "." || n == "*" || n == "**" {
        return SourceReach::ViaContext;
    }
    if !n.contains('/') && glob_matches(&n, "keel.toml") {
        return SourceReach::ViaContext;
    }
    SourceReach::No
}

/// Read one build file's text (plus the project's `.dockerignore` text) into a
/// verdict. Split out from [`analyze`] so the whole decision table is testable
/// without a filesystem.
#[must_use]
pub fn analyze_text(file: &str, text: &str, ignore: &str) -> BuildFile {
    let lines = logical_lines(text);
    let stage_count = lines
        .iter()
        .filter(|l| l.to_ascii_uppercase().starts_with("FROM "))
        .count();
    let ignored = dockerignore_excludes(ignore);
    let mut stage = 0usize;
    let mut reach = Reach::NotReached;
    let mut directive = None;
    let mut in_final = false;
    for line in &lines {
        if line.to_ascii_uppercase().starts_with("FROM ") {
            stage += 1;
            continue;
        }
        let Some(sources) = copy_sources(line) else {
            continue;
        };
        let this_final = stage == stage_count;
        for src in sources {
            match source_reach(&src) {
                SourceReach::Explicit => {
                    reach = Reach::Reached;
                    in_final |= this_final;
                }
                SourceReach::ViaContext if !ignored => {
                    reach = Reach::Reached;
                    in_final |= this_final;
                }
                SourceReach::Variable if reach != Reach::Reached => {
                    reach = Reach::Indeterminate;
                    directive.get_or_insert_with(|| line.clone());
                }
                _ => {}
            }
        }
    }
    if reach == Reach::Reached {
        directive = None;
    }
    BuildFile {
        file: file.to_owned(),
        reach,
        directive,
        reached_in_final_stage: in_final,
    }
}

/// The one-line `keel init` note when a root build file will not ship the
/// policy just written. `None` when there is no build file or all reach it.
#[must_use]
pub fn init_note(project: &Path) -> Option<String> {
    let missing: Vec<String> = analyze(project)
        .into_iter()
        .filter(|b| b.reach == Reach::NotReached)
        .map(|b| b.file)
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "keel \u{25b8} note: {} does not COPY keel.toml \u{2014} add `COPY keel.toml ./` so the \
         policy ships in the image",
        missing.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reach(text: &str) -> Reach {
        analyze_text("Dockerfile", text, "").reach
    }

    #[test]
    fn explicit_copy_of_keel_toml_reaches() {
        assert_eq!(
            reach("FROM python:3.12\nCOPY keel.toml ./\n"),
            Reach::Reached
        );
        assert_eq!(
            reach("FROM python:3.12\nCOPY ./keel.toml /app/keel.toml\n"),
            Reach::Reached
        );
        assert_eq!(reach("FROM python:3.12\nADD keel.toml .\n"), Reach::Reached);
        assert_eq!(
            reach("FROM python:3.12\nCOPY pyproject.toml uv.lock keel.toml ./\n"),
            Reach::Reached
        );
    }

    #[test]
    fn directory_and_glob_copies_reach() {
        assert_eq!(reach("FROM x\nCOPY . /code\n"), Reach::Reached);
        assert_eq!(reach("FROM x\nCOPY ./ /code/\n"), Reach::Reached);
        assert_eq!(reach("FROM x\nCOPY *.toml ./\n"), Reach::Reached);
        assert_eq!(reach("FROM x\nCOPY keel.* ./\n"), Reach::Reached);
        assert_eq!(
            reach("FROM x\nCOPY [\"keel.toml\", \"/app/\"]\n"),
            Reach::Reached
        );
        assert_eq!(
            reach("FROM x\nCOPY --chown=app:app --link . /app\n"),
            Reach::Reached
        );
    }

    #[test]
    fn the_field_incident_shape_does_not_reach() {
        // ai-marketing-hub 2026-09-15: everything but keel.toml.
        let text = "FROM python:3.12-slim\nWORKDIR /code\nCOPY pyproject.toml uv.lock ./\n\
                    COPY packages/ packages/\nCOPY services/ services/\nCOPY agents/ agents/\n\
                    COPY config/ config/\nCOPY prompts/ prompts/\nRUN uv sync --frozen\n";
        let bf = analyze_text("Dockerfile", text, "");
        assert_eq!(bf.reach, Reach::NotReached);
        assert_eq!(bf.directive, None);
    }

    #[test]
    fn line_continuations_and_comments_are_handled() {
        assert_eq!(
            reach(
                "FROM x\n# COPY keel.toml ./\nCOPY pyproject.toml \\\n     keel.toml \\\n     ./\n"
            ),
            Reach::Reached
        );
        assert_eq!(
            reach("FROM x\n# COPY keel.toml ./\nCOPY pyproject.toml ./\n"),
            Reach::NotReached
        );
        // A comment line INSIDE a continuation is dropped by Docker, so the
        // `keel.toml` token on it is not a source — concatenating it would be
        // a false negative on the warning this module exists to raise.
        assert_eq!(
            reach("FROM x\nCOPY pyproject.toml \\\n# keel.toml \\\n     ./\n"),
            Reach::NotReached
        );
        // ...but a real source on a continued line AFTER a dropped comment
        // still counts — dropping the comment must not drop the rest.
        assert_eq!(
            reach("FROM x\nCOPY pyproject.toml \\\n# a note\n     keel.toml ./\n"),
            Reach::Reached
        );
    }

    #[test]
    fn variables_are_indeterminate() {
        let bf = analyze_text("Dockerfile", "FROM x\nARG SRC=.\nCOPY $SRC /app\n", "");
        assert_eq!(bf.reach, Reach::Indeterminate);
        assert_eq!(bf.directive.as_deref(), Some("COPY $SRC /app"));
        assert_eq!(
            reach("FROM x\nCOPY ${SRC}/keel.toml /app/\n"),
            Reach::Indeterminate
        );
    }

    #[test]
    fn dockerignore_can_negate_a_directory_copy() {
        assert_eq!(
            analyze_text("Dockerfile", "FROM x\nCOPY . /app\n", "keel.toml\n").reach,
            Reach::NotReached
        );
        assert_eq!(
            analyze_text("Dockerfile", "FROM x\nCOPY . /app\n", "*.toml\n").reach,
            Reach::NotReached
        );
        assert_eq!(
            analyze_text("Dockerfile", "FROM x\nCOPY . /app\n", "*\n!keel.toml\n").reach,
            Reach::Reached
        );
        assert_eq!(
            analyze_text(
                "Dockerfile",
                "FROM x\nCOPY . /app\n",
                "# comment\n.git\nnode_modules\n"
            )
            .reach,
            Reach::Reached
        );
        // An explicit COPY keel.toml is NOT negated by .dockerignore in practice
        // (BuildKit errors instead) — treat as reached, the build will tell them.
        assert_eq!(
            analyze_text("Dockerfile", "FROM x\nCOPY keel.toml ./\n", "keel.toml\n").reach,
            Reach::Reached
        );
    }

    #[test]
    fn multi_stage_reports_whether_the_final_stage_has_it() {
        let only_builder = "FROM x AS builder\nCOPY . /src\nRUN build\nFROM y\nCOPY --from=builder /src/dist /app\n";
        let bf = analyze_text("Dockerfile", only_builder, "");
        assert_eq!(bf.reach, Reach::Reached);
        assert!(!bf.reached_in_final_stage);
        let final_too = "FROM x AS builder\nCOPY . /src\nFROM y\nCOPY keel.toml /app/\n";
        assert!(analyze_text("Dockerfile", final_too, "").reached_in_final_stage);
        // `COPY --from=` never reaches the build context: ignored as a source.
        assert_eq!(
            reach("FROM x\nCOPY --from=builder /keel.toml /app/\n"),
            Reach::NotReached
        );
    }

    #[test]
    fn analyze_finds_root_build_files_only() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM x\nCOPY keel.toml .\n").unwrap();
        std::fs::write(dir.path().join("Dockerfile.dev"), "FROM x\nCOPY app.py .\n").unwrap();
        std::fs::write(dir.path().join("worker.Dockerfile"), "FROM x\nCOPY . .\n").unwrap();
        std::fs::write(dir.path().join("Containerfile"), "FROM x\n").unwrap();
        std::fs::create_dir(dir.path().join("deploy")).unwrap();
        std::fs::write(dir.path().join("deploy/Dockerfile"), "FROM x\n").unwrap();
        let files: Vec<(String, Reach)> = analyze(dir.path())
            .into_iter()
            .map(|b| (b.file, b.reach))
            .collect();
        assert_eq!(
            files,
            vec![
                ("Containerfile".to_owned(), Reach::NotReached),
                ("Dockerfile".to_owned(), Reach::Reached),
                ("Dockerfile.dev".to_owned(), Reach::NotReached),
                ("worker.Dockerfile".to_owned(), Reach::Reached),
            ]
        );
    }

    #[test]
    fn init_note_fires_only_when_a_root_build_file_misses_keel_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(init_note(dir.path()).is_none(), "no Dockerfile → no note");
        std::fs::write(dir.path().join("Dockerfile"), "FROM x\nCOPY app.py .\n").unwrap();
        assert_eq!(
            init_note(dir.path()).as_deref(),
            Some(
                "keel \u{25b8} note: Dockerfile does not COPY keel.toml \u{2014} add `COPY keel.toml ./` so the policy ships in the image"
            )
        );
        std::fs::write(dir.path().join("Dockerfile"), "FROM x\nCOPY . .\n").unwrap();
        assert!(init_note(dir.path()).is_none());
    }
}
