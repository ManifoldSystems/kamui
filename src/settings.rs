//! User vs project `settings.json` for `disabledSkills`.
//!
//! Two files are consulted (union):
//!   - user:    `<global_config_dir>/settings.json`  (e.g. `~/.config/kamui/settings.json`)
//!   - project: `<project>/.kamui/settings.json`
//!
//! Each file stores `{ "disabledSkills": ["skill-a", ...] }` and preserves any other keys.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::skills::{Skill, SkillSource};

const KEY: &str = "disabledSkills";

#[derive(Debug, Default)]
pub struct DisabledSkillsReport {
    pub disabled: HashSet<String>,
    pub warnings: Vec<String>,
}

pub fn user_settings_path() -> Result<PathBuf> {
    Ok(crate::config::global_config_dir()?.join("settings.json"))
}

pub fn project_settings_path(project_root: &Path) -> PathBuf {
    project_root.join(".kamui/settings.json")
}

/// Union of user + project disabled skills. Use `load_disabled_skills_report` when diagnostics
/// can be shown to the user.
#[allow(dead_code)]
pub fn load_disabled_skills(project_root: &Path) -> HashSet<String> {
    load_disabled_skills_report(project_root).disabled
}

pub fn load_disabled_skills_report(project_root: &Path) -> DisabledSkillsReport {
    let mut report = DisabledSkillsReport::default();
    let mut paths = Vec::new();
    match user_settings_path() {
        Ok(path) => paths.push(path),
        Err(error) => report
            .warnings
            .push(format!("settings: user settings path: {error:#}")),
    }
    paths.push(project_settings_path(project_root));
    for path in paths {
        match read_disabled_from_file(&path) {
            Ok(set) => report.disabled.extend(set),
            Err(error) => report
                .warnings
                .push(format!("settings: {}: {error:#}", path.display())),
        }
    }
    report
}

fn read_disabled_from_file(path: &Path) -> Result<HashSet<String>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(e.into()),
    };
    let value: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("root must be a JSON object"))?;
    let mut set = HashSet::new();
    if let Some(value) = obj.get(KEY) {
        let arr = value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("{KEY} must be an array of strings"))?;
        for (index, item) in arr.iter().enumerate() {
            let s = item
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{KEY}[{index}] must be a string"))?
                .trim()
                .to_ascii_lowercase();
            if !s.is_empty() {
                set.insert(s);
            }
        }
    }
    Ok(set)
}

fn write_disabled_to_file(path: &Path, set: &HashSet<String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut value = match std::fs::read_to_string(path) {
        Ok(c) => serde_json::from_str::<serde_json::Value>(&c).map_err(|e| {
            anyhow::anyhow!("refusing to overwrite malformed {}: {e}", path.display())
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(Default::default())
        }
        Err(e) => return Err(e.into()),
    };
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json must be a JSON object"))?;

    if set.is_empty() {
        obj.remove(KEY);
        // If the file would become empty, remove it rather than leaving `{}`.
        if obj.is_empty() {
            let _ = std::fs::remove_file(path);
            return Ok(());
        }
    } else {
        let mut sorted: Vec<String> = set.iter().cloned().collect();
        sorted.sort();
        obj.insert(
            KEY.to_string(),
            serde_json::Value::Array(sorted.into_iter().map(serde_json::Value::String).collect()),
        );
    }

    let pretty = serde_json::to_string_pretty(&value)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, pretty + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(temporary, path)?;
    Ok(())
}

/// Persist a toggle. Project skills go to the project file, global skills to the user file.
/// When enabling, the name is removed from *both* files so a stale entry in the other scope
/// cannot keep the skill disabled via the union.
pub fn set_skill_disabled(project_root: &Path, skill: &Skill, disabled: bool) -> Result<()> {
    let project_path = project_settings_path(project_root);
    let user_path = user_settings_path()?;

    let is_project = matches!(
        skill.source,
        SkillSource::ProjectKamui | SkillSource::ProjectAgents
    );

    if disabled {
        // Add to the owning scope.
        let target = if is_project {
            &project_path
        } else {
            &user_path
        };
        let mut set = read_disabled_from_file(target)?;
        set.insert(skill.name.clone());
        write_disabled_to_file(target, &set)?;
    } else {
        // Remove from both scopes — union means either file can keep it disabled.
        for path in [&project_path, &user_path] {
            let mut set = read_disabled_from_file(path)?;
            if set.remove(&skill.name) {
                write_disabled_to_file(path, &set)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn tmp_project() -> PathBuf {
        let p = std::env::temp_dir().join(format!("kamui-settings-{}", Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn read_missing_file_is_empty() {
        let p = tmp_project();
        let set = read_disabled_from_file(&p.join("nope.json")).unwrap();
        assert!(set.is_empty());
        fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn write_and_read_round_trips_and_preserves_other_keys() {
        let p = tmp_project();
        let path = p.join("settings.json");
        fs::write(&path, r#"{"other": 123}"#).unwrap();
        let mut set = HashSet::new();
        set.insert("my-skill".to_string());
        write_disabled_to_file(&path, &set).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["other"], 123);
        assert_eq!(raw["disabledSkills"][0], "my-skill");
        let back = read_disabled_from_file(&path).unwrap();
        assert!(back.contains("my-skill"));
        fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn empty_set_removes_key_and_file_when_alone() {
        let p = tmp_project();
        let path = p.join("settings.json");
        let mut set = HashSet::new();
        set.insert("a".to_string());
        write_disabled_to_file(&path, &set).unwrap();
        assert!(path.exists());
        write_disabled_to_file(&path, &HashSet::new()).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn malformed_root_and_disabled_skills_types_are_errors() {
        let p = tmp_project();
        let path = p.join("settings.json");
        for (data, expected) in [
            ("{", "invalid JSON"),
            ("[]", "root must be a JSON object"),
            (r#"{"disabledSkills":true}"#, "must be an array"),
            (r#"{"disabledSkills":["ok",3]}"#, "disabledSkills[1]"),
        ] {
            fs::write(&path, data).unwrap();
            let error = read_disabled_from_file(&path).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
        fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn write_refuses_and_preserves_malformed_file() {
        let p = tmp_project();
        let path = p.join("settings.json");
        fs::write(&path, "{broken").unwrap();
        let error = write_disabled_to_file(&path, &HashSet::from(["x".into()])).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
        fs::remove_dir_all(p).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_reports_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp_project();
        let path = p.join("settings.json");
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let result = read_disabled_from_file(&path);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(result.is_err() || result.unwrap().is_empty());
        fs::remove_dir_all(p).unwrap();
    }
}
