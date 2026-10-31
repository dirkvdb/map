use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::config::{state_dir, write_json};

const MAX_ENTRIES: usize = 50;

#[derive(Debug, Deserialize, Serialize)]
struct RecentFile {
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    epsg: Option<u32>,
}

// Accept the original path-only history as well as entries with open options.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredEntry {
    File(RecentFile),
    Path(PathBuf),
}

/// Successfully opened paths, ordered from most to least recent.
#[derive(Debug, Default)]
pub(crate) struct RecentFiles {
    entries: Vec<RecentFile>,
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

    pub(crate) fn load_from(storage: Option<PathBuf>) -> Self {
        let mut recent = Self { storage, ..Self::default() };
        if let Some(path) = &recent.storage {
            match read_entries(path) {
                Ok(entries) => recent.entries = entries,
                Err(error) => log::warn!("Could not load recent files: {error:#}"),
            }
        }
        recent
    }

    /// Records a successfully opened path; callers must only call this after opening succeeds.
    /// Uses a canonical path when possible, otherwise an absolute path. Persistence errors
    /// are returned after updating memory; without storage, history remains in memory only.
    pub(crate) fn record(&mut self, path: &Path, epsg: Option<u32>) -> anyhow::Result<()> {
        let path = normalize(path)?;
        self.entries.retain(|entry| entry.path != path);
        self.entries.insert(0, RecentFile { path, epsg });
        self.entries.truncate(MAX_ENTRIES);
        if let Some(storage) = &self.storage {
            write_json(storage, &serde_json::to_vec_pretty(&self.entries)?)?;
        }
        Ok(())
    }

    pub(crate) fn paths(&self) -> Vec<PathBuf> {
        self.entries.iter().map(|entry| entry.path.clone()).collect()
    }

    pub(crate) fn epsg(&self, path: &Path) -> Option<u32> {
        let path = normalize(path).ok()?;
        self.entries.iter().find(|entry| entry.path == path)?.epsg
    }
}

fn read_entries(path: &Path) -> anyhow::Result<Vec<RecentFile>> {
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("Could not read {}", path.display())),
    };
    let entries: Vec<StoredEntry> = serde_json::from_slice(&content).with_context(|| format!("Could not parse {}", path.display()))?;
    let mut files: Vec<RecentFile> = Vec::new();
    for entry in entries {
        let mut entry = match entry {
            StoredEntry::File(file) => file,
            StoredEntry::Path(path) => RecentFile { path, epsg: None },
        };
        entry.path = normalize(&entry.path)?;
        if !files.iter().any(|file| file.path == entry.path) {
            files.push(entry);
        }
        if files.len() == MAX_ENTRIES {
            break;
        }
    }
    Ok(files)
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
            recent.record(path, None).unwrap();
        }
        assert_eq!(recent.paths(), &[second, third, first]);
    }

    #[test]
    fn record_keeps_only_the_fifty_most_recent_paths() {
        let directory = tempfile::tempdir().unwrap();
        let mut recent = RecentFiles::default();
        for index in 0..60 {
            recent.record(&directory.path().join(format!("{index}.tif")), None).unwrap();
        }
        let expected: Vec<_> = (10..60).rev().map(|index| directory.path().join(format!("{index}.tif"))).collect();
        assert_eq!(recent.paths(), expected);
    }

    #[test]
    fn history_roundtrips_through_json_in_a_new_directory() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("state/map/recent-files.json");
        let mut recent = RecentFiles::load_from(Some(storage.clone()));
        recent.record(&directory.path().join("first.tif"), Some(31370)).unwrap();
        recent.record(&directory.path().join("second.tif"), None).unwrap();
        let loaded = RecentFiles::load_from(Some(storage.clone()));
        assert_eq!(loaded.paths(), recent.paths());
        assert_eq!(loaded.epsg(&directory.path().join("first.tif")), Some(31370));
        assert_eq!(loaded.epsg(&directory.path().join("second.tif")), None);
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
            recent.record(&directory.path().join("opened.tif"), None).unwrap();
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
        assert!(recent.record(&opened, None).is_err());
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
    fn legacy_paths_and_projection_entries_can_be_loaded_and_saved_together() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("recent-files.json");
        let legacy = directory.path().join("legacy.tif");
        let projected = directory.path().join("projected.tif");
        write_json(
            &storage,
            &serde_json::to_vec(&serde_json::json!([legacy, {"path": projected, "epsg": 31370}])).unwrap(),
        )
        .unwrap();
        let mut recent = RecentFiles::load_from(Some(storage.clone()));
        assert_eq!(recent.paths(), [legacy.clone(), projected.clone()]);
        assert_eq!(recent.epsg(&legacy), None);
        assert_eq!(recent.epsg(&projected), Some(31370));
        recent.record(&legacy, Some(4326)).unwrap();
        let reloaded = RecentFiles::load_from(Some(storage));
        assert_eq!(reloaded.epsg(&legacy), Some(4326));
        assert_eq!(reloaded.epsg(&projected), Some(31370));
    }

    #[test]
    fn relative_and_absolute_existing_paths_share_a_canonical_entry() {
        let mut recent = RecentFiles::default();
        recent.record(Path::new("./Cargo.toml"), Some(4326)).unwrap();
        let canonical = fs::canonicalize("Cargo.toml").unwrap();
        assert_eq!(recent.epsg(&canonical), Some(4326));
        recent.record(&canonical, Some(3857)).unwrap();
        assert_eq!(recent.paths(), &[canonical]);
        assert_eq!(recent.epsg(Path::new("Cargo.toml")), Some(3857));
    }

    #[test]
    fn missing_relative_path_is_retained_as_absolute() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("removed.tif");
        let mut recent = RecentFiles::default();
        recent.record(&path, None).unwrap();
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
        recent.record(&link, Some(4326)).unwrap();
        assert_eq!(recent.epsg(&target), Some(4326));
        recent.record(&target, Some(4326)).unwrap();
        assert_eq!(recent.paths(), &[fs::canonicalize(target).unwrap()]);
    }
}
