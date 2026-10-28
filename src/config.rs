use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail};
use directories::{BaseDirs, ProjectDirs};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug)]
pub(crate) struct ConfigPaths {
    config: PathBuf,
    state: PathBuf,
}

impl ConfigPaths {
    pub(crate) fn new(config: PathBuf, state: PathBuf) -> Self {
        Self { config, state }
    }

    pub(crate) fn discover() -> anyhow::Result<Self> {
        let project = ProjectDirs::from("", "", "map").context("The configuration directory is unavailable")?;
        Ok(Self::new(project.config_dir().join("config.json"), state_dir()?.join("config.json")))
    }

    fn read_path(&self) -> anyhow::Result<&Path> {
        if exists(&self.state)? { Ok(&self.state) } else { Ok(&self.config) }
    }

    pub(crate) fn read(&self) -> anyhow::Result<Option<Value>> {
        let path = self.read_path()?;
        match fs::read(path) {
            Ok(content) => serde_json::from_slice(&content)
                .with_context(|| format!("Could not parse {}", path.display()))
                .map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
        }
    }

    pub(crate) fn update(&self, value: &impl Serialize) -> anyhow::Result<()> {
        // Parse and merge before any write or fallback, preserving unknown settings
        // and refusing to overwrite malformed user configuration.
        let mut current = self.read()?.unwrap_or_else(|| serde_json::json!({}));
        let updates = serde_json::to_value(value)?;
        let (Some(current), Some(updates)) = (current.as_object_mut(), updates.as_object()) else {
            bail!("Settings must be a JSON object");
        };
        current.extend(updates.clone());
        let data = serde_json::to_vec_pretty(current)?;
        self.write(&data)
    }

    fn write(&self, data: &[u8]) -> anyhow::Result<()> {
        if exists(&self.state)? {
            if !is_writable(&self.state) {
                bail!("Settings are not writable: {}", self.state.display());
            }
            return write_json(&self.state, data);
        }
        if is_writable(&self.config) {
            match write_json(&self.config, data) {
                Ok(()) => return Ok(()),
                Err(error) if self.config != self.state && is_writable(&self.state) => {
                    log::warn!("Could not save configuration; using {}: {error:#}", self.state.display());
                }
                Err(error) => return Err(error),
            }
        }
        if !is_writable(&self.state) {
            bail!("Settings are not writable: {} or {}", self.config.display(), self.state.display());
        }
        write_json(&self.state, data)
    }
}

pub(crate) fn state_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().join(".local/state")))
        .map(|path| path.join("map"))
        .context("The state directory is unavailable")
}

fn exists(path: &Path) -> anyhow::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("Could not inspect {}", path.display())),
    }
}

fn parent_is_writable(path: &Path) -> bool {
    let Some(mut parent) = path.parent() else { return false };
    loop {
        match fs::metadata(parent) {
            Ok(metadata) => return metadata.is_dir() && permissions_allow_write(&metadata, true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(ancestor) = parent.parent() else { return false };
                parent = ancestor;
            }
            Err(_) => return false,
        }
    }
}

fn is_writable(path: &Path) -> bool {
    if !parent_is_writable(path) {
        return false;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::metadata(path).is_ok_and(|target| target.is_file() && permissions_allow_write(&target, false)),
        Ok(metadata) => metadata.is_file() && permissions_allow_write(&metadata, false),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

#[cfg(unix)]
fn permissions_allow_write(metadata: &fs::Metadata, directory: bool) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    mode & 0o222 != 0 && (!directory || mode & 0o111 != 0)
}

#[cfg(not(unix))]
fn permissions_allow_write(metadata: &fs::Metadata, _: bool) -> bool {
    !metadata.permissions().readonly()
}

pub(crate) fn write_json(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let directory = path.parent().context("The settings path has no parent directory")?;
    fs::create_dir_all(directory).with_context(|| format!("Could not create {}", directory.display()))?;
    // A same-directory temporary file keeps replacement atomic and private (0600 on Unix).
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(data)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).with_context(|| format!("Could not save {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(directory: &Path) -> ConfigPaths {
        ConfigPaths::new(directory.join("config/map/config.json"), directory.join("state/map/config.json"))
    }

    #[test]
    fn writes_a_single_config_file_and_preserves_unrelated_settings() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(directory.path());
        paths.update(&serde_json::json!({"other": {"enabled": true}})).unwrap();
        paths.update(&serde_json::json!({"mode": "dark"})).unwrap();
        assert_eq!(paths.read().unwrap().unwrap(), serde_json::json!({"other": {"enabled": true}, "mode": "dark"}));
        assert!(!paths.state.exists());
    }

    #[test]
    fn state_override_takes_precedence_for_reads_and_writes() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(directory.path());
        paths.update(&serde_json::json!({"mode": "light"})).unwrap();
        write_json(&paths.state, br#"{"mode":"dark","other":1}"#).unwrap();
        paths.update(&serde_json::json!({"light_theme":"Ayu Light"})).unwrap();
        assert_eq!(paths.read().unwrap().unwrap()["mode"], "dark");
        assert_eq!(paths.read().unwrap().unwrap()["other"], 1);
        assert_eq!(serde_json::from_slice::<Value>(&fs::read(&paths.config).unwrap()).unwrap()["mode"], "light");
    }

    #[test]
    fn malformed_or_non_object_config_is_not_overwritten_or_hidden_by_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(directory.path());
        fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        for content in ["{invalid", "[]", "null"] {
            fs::write(&paths.config, content).unwrap();
            assert!(paths.update(&serde_json::json!({"mode":"dark"})).is_err());
            assert_eq!(fs::read_to_string(&paths.config).unwrap(), content);
            assert!(!paths.state.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_only_file_and_nix_style_symlink_use_a_state_override() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for use_symlink in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let paths = paths(directory.path());
            fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
            let original = br#"{"mode":"light","other":42}"#;
            let target = if use_symlink {
                directory.path().join("store.json")
            } else {
                paths.config.clone()
            };
            fs::write(&target, original).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).unwrap();
            if use_symlink {
                symlink(&target, &paths.config).unwrap();
            }
            paths.update(&serde_json::json!({"mode":"dark"})).unwrap();
            assert_eq!(fs::read(&target).unwrap(), original);
            assert_eq!(paths.read().unwrap().unwrap(), serde_json::json!({"mode":"dark","other":42}));
            assert!(paths.state.is_file());
            if use_symlink {
                assert!(paths.config.is_symlink());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_only_override_does_not_fall_back_to_the_main_config() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let paths = paths(directory.path());
        paths.update(&serde_json::json!({"mode":"light"})).unwrap();
        write_json(&paths.state, br#"{"mode":"dark"}"#).unwrap();
        fs::set_permissions(&paths.state, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(paths.update(&serde_json::json!({"mode":"system"})).is_err());
        assert_eq!(paths.read().unwrap().unwrap()["mode"], "dark");
        assert_eq!(serde_json::from_slice::<Value>(&fs::read(&paths.config).unwrap()).unwrap()["mode"], "light");
    }
}
