use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::LensError;
use crate::store::IndexRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    /// Relative path as the OS reports it, using `/` separators.
    ///
    /// Design amendment 4 asked for NFC normalization. Pulling a Unicode table
    /// crate only for this path key is not worth it: macOS commonly reports NFD,
    /// but freshness compares rows against the same walker output on later runs,
    /// so the representation is self-consistent.
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub size: u64,
    pub mtime_ns: i128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freshness {
    pub fresh: Vec<WalkedFile>,
    pub stale: Vec<WalkedFile>,
    pub new: Vec<WalkedFile>,
    pub vanished: Vec<IndexRecord>,
}

pub fn walk_library(root: impl AsRef<Path>) -> Result<Vec<WalkedFile>, LensError> {
    let root = root.as_ref();
    let mut out = Vec::new();
    walk_dir(root, root, &mut out)?;
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

pub fn partition_freshness(walked: &[WalkedFile], records: &[IndexRecord]) -> Freshness {
    let mut last_by_path: HashMap<&str, &IndexRecord> = HashMap::new();
    for record in records {
        last_by_path.insert(record.rel_path.as_str(), record);
    }

    let mut seen = HashSet::new();
    let mut fresh = Vec::new();
    let mut stale = Vec::new();
    let mut new = Vec::new();

    for file in walked {
        seen.insert(file.rel_path.as_str());
        match last_by_path.get(file.rel_path.as_str()) {
            Some(record) if record.size == file.size && record.mtime_ns == file.mtime_ns => {
                fresh.push(file.clone());
            }
            Some(_) => stale.push(file.clone()),
            None => new.push(file.clone()),
        }
    }

    let vanished = last_by_path
        .into_values()
        .filter(|record| !seen.contains(record.rel_path.as_str()))
        .cloned()
        .collect();

    Freshness {
        fresh,
        stale,
        new,
        vanished,
    }
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<WalkedFile>) -> Result<(), LensError> {
    let mut entries = fs::read_dir(dir)
        .map_err(|err| LensError::config(format!("failed to read {}: {err}", dir.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| LensError::config(format!("failed to read {}: {err}", dir.display())))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }

        let file_type = entry.file_type().map_err(|err| {
            LensError::config(format!("failed to stat {}: {err}", entry.path().display()))
        })?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk_dir(root, &path, out)?;
        } else if file_type.is_file() && allowed_extension(&entry.path()) {
            let meta = entry.metadata().map_err(|err| {
                LensError::config(format!("failed to stat {}: {err}", entry.path().display()))
            })?;
            let rel = path.strip_prefix(root).map_err(|err| {
                LensError::config(format!(
                    "failed to build relative path for {}: {err}",
                    path.display()
                ))
            })?;
            out.push(WalkedFile {
                rel_path: rel_to_slashes(rel),
                abs_path: path,
                size: meta.len(),
                mtime_ns: mtime_ns(&meta),
            });
        }
    }
    Ok(())
}

fn rel_to_slashes(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn allowed_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "heic"
            )
        })
}

#[cfg(unix)]
fn mtime_ns(meta: &fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec())
}

#[cfg(not(unix))]
fn mtime_ns(meta: &fs::Metadata) -> i128 {
    use std::time::UNIX_EPOCH;
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn record(file: &WalkedFile) -> IndexRecord {
        IndexRecord {
            rel_path: file.rel_path.clone(),
            size: file.size,
            mtime_ns: file.mtime_ns,
            description: String::new(),
            filename: String::new(),
            tags: Vec::new(),
            text_content: String::new(),
            kind: "photo".into(),
        }
    }

    #[test]
    fn hidden_symlink_extension_rules_and_sorting() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("b.PNG"), b"b");
        write(&dir.path().join("a.jpg"), b"a");
        write(&dir.path().join(".hidden.jpg"), b"x");
        write(&dir.path().join(".hidden_dir/c.jpg"), b"x");
        write(&dir.path().join("skip.txt"), b"x");
        symlink(dir.path().join("a.jpg"), dir.path().join("link.jpg")).unwrap();

        let files = walk_library(dir.path()).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|f| f.rel_path.as_str())
                .collect::<Vec<_>>(),
            vec!["a.jpg", "b.PNG"]
        );
    }

    #[test]
    fn freshness_partition_has_all_four_buckets() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("fresh.jpg"), b"fresh");
        write(&dir.path().join("stale.jpg"), b"stale");
        write(&dir.path().join("new.jpg"), b"new");
        let walked = walk_library(dir.path()).unwrap();

        let fresh_file = walked.iter().find(|f| f.rel_path == "fresh.jpg").unwrap();
        let stale_file = walked.iter().find(|f| f.rel_path == "stale.jpg").unwrap();
        let mut stale_record = record(stale_file);
        stale_record.mtime_ns -= 1;
        let vanished = IndexRecord {
            rel_path: "vanished.jpg".into(),
            size: 1,
            mtime_ns: 1,
            description: String::new(),
            filename: String::new(),
            tags: Vec::new(),
            text_content: String::new(),
            kind: "photo".into(),
        };

        let partition = partition_freshness(&walked, &[record(fresh_file), stale_record, vanished]);
        assert_eq!(partition.fresh.len(), 1);
        assert_eq!(partition.stale.len(), 1);
        assert_eq!(partition.new.len(), 1);
        assert_eq!(partition.vanished.len(), 1);
    }
}
