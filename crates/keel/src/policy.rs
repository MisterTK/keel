use keel_core_api::{ErrorCode, KeelError};
use std::path::Path;

/// Load `<dir>/keel.toml` as JSON (per `contracts/policy.schema.json`), or
/// Level 0 defaults (an empty policy — `Engine::configure` treats an absent
/// `[target]` table as "nothing wrapped yet", the same fallback
/// `python/keel`'s `load_policy`/`node/keel`'s `loadPolicy` use) if the file
/// is absent.
pub(crate) fn load(dir: &Path) -> Result<serde_json::Value, KeelError> {
    let path = dir.join("keel.toml");
    if !path.is_file() {
        return Ok(serde_json::json!({}));
    }
    let contents = std::fs::read_to_string(&path).map_err(|err| KeelError {
        code: ErrorCode::PolicyInvalid,
        message: format!(
            "keel.toml at {} is present but could not be read: {err}. Fix the file's \
             permissions/path, or remove it to fall back to Level 0 defaults.",
            path.display()
        ),
    })?;
    toml::from_str(&contents).map_err(|err| KeelError {
        code: ErrorCode::PolicyInvalid,
        message: format!("keel.toml at {} is not valid TOML: {err}", path.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_level0_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let policy = load(dir.path()).expect("defaults never fail");
        assert_eq!(policy, serde_json::json!({}));
    }

    #[test]
    fn valid_toml_parses_to_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("keel.toml"),
            "[target.\"orders-api\"]\nretry = { attempts = 3 }\n",
        )
        .unwrap();
        let policy = load(dir.path()).expect("valid toml parses");
        assert_eq!(
            policy["target"]["orders-api"]["retry"]["attempts"],
            serde_json::json!(3)
        );
    }

    #[test]
    fn invalid_toml_is_keel_e001() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keel.toml"), "not = [valid").unwrap();
        let err = load(dir.path()).unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyInvalid);
    }
}
