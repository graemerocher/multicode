use std::{io, path::Path, time::SystemTime};

pub fn latest_modified_time(
    path: &Path,
    is_dir: bool,
    exclude: &[String],
) -> io::Result<Option<SystemTime>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !is_dir || !metadata.is_dir() {
        return Ok(metadata.modified().ok());
    }

    let mut latest = None;
    let mut stack = vec![path.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        for entry in entries {
            let entry = entry?;
            let entry_path = entry.path();
            let relative_path = entry_path
                .strip_prefix(path)
                .expect("entry should stay within walked root");
            if is_excluded_relative_path(relative_path, exclude) {
                continue;
            }
            let entry_metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            latest = max_modified_time(latest, entry_metadata.modified().ok());
            if entry_metadata.is_dir() {
                stack.push(entry_path);
            }
        }
    }
    Ok(latest)
}

pub fn is_excluded_relative_path(path: &Path, exclude: &[String]) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    exclude.iter().any(|pattern| {
        let trimmed = pattern.trim_matches('/');
        !trimmed.is_empty()
            && (normalized == trimmed || normalized.starts_with(&format!("{trimmed}/")))
    })
}

pub fn max_modified_time(
    current: Option<SystemTime>,
    candidate: Option<SystemTime>,
) -> Option<SystemTime> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.max(candidate)),
        (Some(current), None) => Some(current),
        (None, Some(candidate)) => Some(candidate),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn latest_modified_time_returns_none_for_missing_path() {
        let missing = std::env::temp_dir().join(format!(
            "multicode-tree-scan-missing-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        assert_eq!(latest_modified_time(&missing, true, &[]).unwrap(), None);
    }

    #[test]
    fn latest_modified_time_tracks_newest_nested_entry() {
        let temp_root = std::env::temp_dir().join(format!(
            "multicode-tree-scan-dir-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp_root.join("nested")).unwrap();
        let old_file = temp_root.join("old.txt");
        let new_file = temp_root.join("nested/new.txt");
        std::fs::write(&old_file, "old").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        std::fs::write(&new_file, "new").unwrap();

        let latest = latest_modified_time(&temp_root, true, &[])
            .unwrap()
            .unwrap();
        let newest_file_mtime = std::fs::metadata(&new_file).unwrap().modified().unwrap();
        assert!(latest >= newest_file_mtime);

        std::fs::remove_dir_all(&temp_root).unwrap();
    }

    #[test]
    fn latest_modified_time_honors_excluded_subtrees() {
        let temp_root = std::env::temp_dir().join(format!(
            "multicode-tree-scan-exclude-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp_root.join("included")).unwrap();
        std::fs::create_dir_all(temp_root.join("excluded/subdir")).unwrap();
        let included = temp_root.join("included/file.txt");
        let excluded = temp_root.join("excluded/subdir/file.txt");
        std::fs::write(&included, "included").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        std::fs::write(&excluded, "excluded").unwrap();

        let latest = latest_modified_time(&temp_root, true, &["excluded".to_string()])
            .unwrap()
            .unwrap();
        let included_mtime = std::fs::metadata(&included).unwrap().modified().unwrap();
        let excluded_mtime = std::fs::metadata(&excluded).unwrap().modified().unwrap();
        assert!(latest >= included_mtime);
        assert!(latest < excluded_mtime);

        std::fs::remove_dir_all(&temp_root).unwrap();
    }

    #[test]
    fn excluded_relative_path_matches_exact_paths_and_descendants() {
        let exclude = vec![".multicode/remote".to_string()];
        assert!(is_excluded_relative_path(
            Path::new(".multicode/remote"),
            &exclude
        ));
        assert!(is_excluded_relative_path(
            Path::new(".multicode/remote/relay/file.sock"),
            &exclude
        ));
        assert!(!is_excluded_relative_path(
            Path::new(".multicode"),
            &exclude
        ));
        assert!(!is_excluded_relative_path(
            Path::new("workspace/file.txt"),
            &exclude
        ));
    }

    #[test]
    fn max_modified_time_keeps_latest_candidate() {
        let now = SystemTime::now();
        let later = now.checked_add(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(max_modified_time(Some(now), Some(later)), Some(later));
        assert_eq!(max_modified_time(Some(now), None), Some(now));
        assert_eq!(max_modified_time(None, Some(later)), Some(later));
    }
}
