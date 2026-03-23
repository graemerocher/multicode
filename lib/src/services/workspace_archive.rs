use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceArchiveFormat {
    TarZstd,
    TarXz,
    Zip,
}

impl WorkspaceArchiveFormat {
    pub const fn suffix(&self) -> &'static str {
        match self {
            WorkspaceArchiveFormat::TarZstd => ".tar.zstd",
            WorkspaceArchiveFormat::TarXz => ".tar.xz",
            WorkspaceArchiveFormat::Zip => ".zip",
        }
    }

    pub const fn all() -> [WorkspaceArchiveFormat; 3] {
        [
            WorkspaceArchiveFormat::TarZstd,
            WorkspaceArchiveFormat::TarXz,
            WorkspaceArchiveFormat::Zip,
        ]
    }
}

/// Archived workspaces are compressed. This is the compression metadata gleaned from the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveWorkspaceEntry {
    /// The workspace key (folder name if it was uncompressed).
    pub key: String,
    /// The full file name with extension.
    pub file_name: String,
    /// The archive format.
    pub format: WorkspaceArchiveFormat,
}

impl ArchiveWorkspaceEntry {
    pub fn parse(path: &Path) -> Option<ArchiveWorkspaceEntry> {
        let file_name = path.file_name()?.to_string_lossy();
        for format in WorkspaceArchiveFormat::all() {
            if let Some(key) = file_name.strip_suffix(format.suffix())
                && !key.is_empty()
            {
                return Some(ArchiveWorkspaceEntry {
                    key: key.to_string(),
                    file_name: file_name.into_owned(),
                    format,
                });
            }
        }
        None
    }

    pub fn new(key: impl Into<String>, format: WorkspaceArchiveFormat) -> ArchiveWorkspaceEntry {
        let key = key.into();
        let file_name = format!("{}{}", key, format.suffix());
        ArchiveWorkspaceEntry {
            key,
            file_name,
            format,
        }
    }

    pub fn to_path(&self, workspace_root: &Path) -> PathBuf {
        workspace_root.join(&self.file_name)
    }

    pub fn to_isolate_path(&self, workspace_root: &Path) -> PathBuf {
        workspace_root
            .join(".multicode")
            .join("isolate")
            .join(&self.file_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_workspace_entry_parse_matches_supported_suffixes() {
        let tar_zstd = ArchiveWorkspaceEntry::parse(Path::new("alpha.tar.zstd"))
            .expect("tar.zstd archive should parse");
        assert_eq!(tar_zstd.key, "alpha");
        assert_eq!(tar_zstd.format, WorkspaceArchiveFormat::TarZstd);

        let tar_xz = ArchiveWorkspaceEntry::parse(Path::new("beta.tar.xz"))
            .expect("tar.xz archive should parse");
        assert_eq!(tar_xz.key, "beta");
        assert_eq!(tar_xz.format, WorkspaceArchiveFormat::TarXz);

        let zip =
            ArchiveWorkspaceEntry::parse(Path::new("gamma.zip")).expect("zip archive should parse");
        assert_eq!(zip.key, "gamma");
        assert_eq!(zip.format, WorkspaceArchiveFormat::Zip);
    }

    #[test]
    fn archive_workspace_entry_parse_rejects_unsupported_names() {
        assert!(ArchiveWorkspaceEntry::parse(Path::new("plain-dir")).is_none());
        assert!(ArchiveWorkspaceEntry::parse(Path::new("alpha.tar.gz")).is_none());
        assert!(ArchiveWorkspaceEntry::parse(Path::new(".hidden.tar.zstd")).is_some());
        assert!(ArchiveWorkspaceEntry::parse(Path::new(".tar.zstd")).is_none());
    }

    #[test]
    fn archive_workspace_entry_new_builds_file_name_and_paths() {
        let entry = ArchiveWorkspaceEntry::new("alpha", WorkspaceArchiveFormat::TarZstd);
        let root = Path::new("/tmp/workspaces");

        assert_eq!(entry.key, "alpha");
        assert_eq!(entry.file_name, "alpha.tar.zstd");
        assert_eq!(
            entry.to_path(root),
            PathBuf::from("/tmp/workspaces/alpha.tar.zstd")
        );
        assert_eq!(
            entry.to_isolate_path(root),
            PathBuf::from("/tmp/workspaces/.multicode/isolate/alpha.tar.zstd")
        );
    }

    #[test]
    fn workspace_archive_format_suffixes_match_expected_names() {
        assert_eq!(WorkspaceArchiveFormat::TarZstd.suffix(), ".tar.zstd");
        assert_eq!(WorkspaceArchiveFormat::TarXz.suffix(), ".tar.xz");
        assert_eq!(WorkspaceArchiveFormat::Zip.suffix(), ".zip");
    }
}
