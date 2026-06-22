use std::{collections::HashMap, io::ErrorKind, path::Path};

use anyhow::{Context, Result};
use log::{debug, info, warn};

pub type Results = HashMap<String, u64>;

pub fn load(path: &Path) -> Results {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str::<Results>(&contents) {
            Ok(results) => {
                debug!(
                    "Loaded {} cached result(s) from {}",
                    results.len(),
                    path.display()
                );
                results
            }
            Err(e) => {
                warn!(
                    "Cache at {} is corrupt ({}); starting with an empty cache.",
                    path.display(),
                    e
                );
                Results::new()
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => {
            info!("No cache file at {}; starting fresh.", path.display());
            Results::new()
        }
        Err(e) => {
            warn!(
                "Failed to read cache at {} ({}); starting with an empty cache.",
                path.display(),
                e
            );
            Results::new()
        }
    }
}

/// Atomically persists `results` to `path` (write to a temp file, then rename).
pub fn save(path: &Path, results: &Results) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache directory {}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(results).context("failed to serialize results")?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)
        .with_context(|| format!("failed to write temporary file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to move cache file into place at {}", path.display()))?;

    debug!("Saved {} result(s) to {}", results.len(), path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");

        let mut results = Results::new();
        results.insert("foo".to_string(), 42);
        results.insert("bar".to_string(), 0);

        save(&path, &results).unwrap();

        let loaded = load(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("foo").copied(), Some(42));
        assert_eq!(loaded.get("bar").copied(), Some(0));
    }

    #[test]
    fn load_missing_file_is_empty() {
        let loaded = load(Path::new("/this/path/definitely/does/not/exist.json"));
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_corrupt_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, b"definitely not json").unwrap();

        assert!(load(&path).is_empty());
    }
}
