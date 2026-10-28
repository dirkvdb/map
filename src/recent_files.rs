use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context as _;

use crate::config::{state_dir, write_json};

const MAX_ENTRIES: usize = 50;

/// Successfully opened paths, ordered from most to least recent.
#[derive(Debug, Default)]
pub(crate) struct RecentFiles {
    paths: Vec<PathBuf>,
    storage: Option<PathBuf>,
}

impl RecentFiles {
    /// Loads history from the state directory, logging errors without preventing startup.
    pub(crate) fn load() -> Self {
        let storage = match state_dir() {
            Ok(directory) => Some(directory.join("recent-files.json")),
            Err(error) => {
                log::warn!("Could not locate recent files storage: {error:#}");
                None
            }
        };
        Self::load_from(storage)
    }

    fn load_from(storage: Option<PathBuf>) -> Self {
        let mut recent = Self { storage, ..Self::default() };
        if let Some(path) = &recent.storage {
            match read_entries(path) {
                Ok(paths) => recent.paths = paths,
                Err(error) => log::warn!("Could not load recent files: {error:#}"),
            }
        }
        recent
    }

    /// Records a successfully opened path; callers must only call this after opening succeeds.
    /// Uses a canonical path when possible, otherwise an absolute path. Persistence errors
    /// are returned after updating memory; without storage, history remains in memory only.
    pub(crate) fn record(&mut self, path: &Path) -> anyhow::Result<()> {
        let path = normalize(path)?;
        self.paths.retain(|entry| entry != &path);
        self.paths.insert(0, path);
        self.paths.truncate(MAX_ENTRIES);
        if let Some(storage) = &self.storage {
            write_json(storage, &serde_json::to_vec_pretty(&self.paths)?)?;
        }
        Ok(())
    }

    pub(crate) fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

fn read_entries(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("Could not read {}", path.display())),
    };
    let entries: Vec<PathBuf> = serde_json::from_slice(&content).with_context(|| format!("Could not parse {}", path.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = normalize(&entry)?;
        if !paths.contains(&entry) {
            paths.push(entry);
        }
        if paths.len() == MAX_ENTRIES {
            break;
        }
    }
    Ok(paths)
}

fn normalize(path: &Path) -> anyhow::Result<PathBuf> {
    fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .with_context(|| format!("Could not resolve {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_moves_duplicates_to_the_front() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.tif");
        let second = directory.path().join("second.tif");
        let third = directory.path().join("third.tif");
        let mut recent = RecentFiles::default();
        for path in [&first, &second, &third, &second] {
            recent.record(path).unwrap();
        }
        assert_eq!(recent.paths(), &[second, third, first]);
    }

    #[test]
    fn record_keeps_only_the_fifty_most_recent_paths() {
        let directory = tempfile::tempdir().unwrap();
        let mut recent = RecentFiles::default();
        for index in 0..60 {
            recent.record(&directory.path().join(format!("{index}.tif"))).unwrap();
        }
        let expected: Vec<_> = (10..60).rev().map(|index| directory.path().join(format!("{index}.tif"))).collect();
        assert_eq!(recent.paths(), expected);
    }

    #[test]
    fn history_roundtrips_through_json_in_a_new_directory() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("state/map/recent-files.json");
        let mut recent = RecentFiles::load_from(Some(storage.clone()));
        recent.record(&directory.path().join("first.tif")).unwrap();
        recent.record(&directory.path().join("second.tif")).unwrap();
        let loaded = RecentFiles::load_from(Some(storage.clone()));
        assert_eq!(loaded.paths(), recent.paths());
        assert_eq!(serde_json::from_slice::<Vec<PathBuf>>(&fs::read(storage).unwrap()).unwrap(), recent.paths());
    }

    #[test]
    fn missing_history_starts_empty_without_creating_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("recent-files.json");
        let recent = RecentFiles::load_from(Some(storage.clone()));
        assert!(recent.paths().is_empty());
        assert!(!storage.exists());
        assert_eq!(recent.storage.as_ref(), Some(&storage));
    }

    #[test]
    fn malformed_history_falls_back_to_empty_and_can_be_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("recent-files.json");
        for content in ["{invalid", "{}", "null", "[1]"] {
            fs::write(&storage, content).unwrap();
            let mut recent = RecentFiles::load_from(Some(storage.clone()));
            assert!(recent.paths().is_empty());
            assert_eq!(fs::read_to_string(&storage).unwrap(), content);
            recent.record(&directory.path().join("opened.tif")).unwrap();
            assert_eq!(RecentFiles::load_from(Some(storage.clone())).paths(), recent.paths());
        }
    }

    #[test]
    fn unreadable_history_falls_back_and_failed_save_still_updates_memory() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().to_path_buf();
        let mut recent = RecentFiles::load_from(Some(storage));
        assert!(recent.paths().is_empty());
        let opened = directory.path().join("opened.tif");
        assert!(recent.record(&opened).is_err());
        assert_eq!(recent.paths(), &[opened]);
    }

    #[test]
    fn loaded_entries_are_normalized_deduplicated_and_capped_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("recent-files.json");
        let mut entries = vec![PathBuf::from("Cargo.toml"), std::env::current_dir().unwrap().join("Cargo.toml")];
        entries.extend((0..60).map(|index| directory.path().join(format!("{index}.tif"))));
        write_json(&storage, &serde_json::to_vec(&entries).unwrap()).unwrap();
        let recent = RecentFiles::load_from(Some(storage));
        let mut expected = vec![fs::canonicalize("Cargo.toml").unwrap()];
        expected.extend((0..49).map(|index| directory.path().join(format!("{index}.tif"))));
        assert_eq!(recent.paths(), expected);
    }

    #[test]
    fn relative_and_absolute_existing_paths_share_a_canonical_entry() {
        let mut recent = RecentFiles::default();
        recent.record(Path::new("./Cargo.toml")).unwrap();
        let canonical = fs::canonicalize("Cargo.toml").unwrap();
        recent.record(&canonical).unwrap();
        assert_eq!(recent.paths(), &[canonical]);
    }

    #[test]
    fn missing_relative_path_is_retained_as_absolute() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("removed.tif");
        let mut recent = RecentFiles::default();
        recent.record(&path).unwrap();
        assert_eq!(recent.paths(), &[std::path::absolute(path).unwrap()]);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_target_share_a_canonical_entry() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.tif");
        let link = directory.path().join("link.tif");
        fs::write(&target, []).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut recent = RecentFiles::default();
        recent.record(&link).unwrap();
        recent.record(&target).unwrap();
        assert_eq!(recent.paths(), &[fs::canonicalize(target).unwrap()]);
    }
}
