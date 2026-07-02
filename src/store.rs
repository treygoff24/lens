use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Error, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::LensError;

const SCHEMA_VERSION: u32 = 1;
const PROMPT_VERSION: u32 = 1;
const NORMALIZER_VERSION: u32 = 1;
const STALE_LOCK_AFTER: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexRecord {
    pub rel_path: String,
    pub size: u64,
    pub mtime_ns: i128,
    pub description: String,
    pub filename: String,
    pub tags: Vec<String>,
    pub text_content: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreMeta {
    pub library_path: String,
    pub model: String,
    pub schema_version: u32,
    pub prompt_version: u32,
    pub normalizer_version: u32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedIndex {
    pub records: Vec<IndexRecord>,
    pub stale_all: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

#[derive(Debug)]
pub struct StoreLock {
    path: PathBuf,
}

impl Store {
    pub fn open_at(dir: impl AsRef<Path>) -> Result<Self, LensError> {
        fs::create_dir_all(dir.as_ref()).map_err(|err| {
            LensError::config(format!(
                "failed to create index directory {}: {err}",
                dir.as_ref().display()
            ))
        })?;
        Ok(Self {
            dir: dir.as_ref().to_path_buf(),
        })
    }

    pub fn open_for_library(library_path: impl AsRef<Path>) -> Result<Self, LensError> {
        let canonical = fs::canonicalize(library_path.as_ref()).map_err(|err| {
            LensError::config(format!(
                "failed to resolve library path {}: {err}",
                library_path.as_ref().display()
            ))
        })?;
        let hash = hex_prefix(&canonical.to_string_lossy());
        Self::open_at(data_home().join("lens/libraries").join(hash))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn index_path(&self) -> PathBuf {
        self.dir.join("index.jsonl")
    }

    pub fn meta_path(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("index.lock")
    }

    /// Acquires an advisory per-library lock (`index.lock`) via `create_new`.
    ///
    /// Accepted limitations:
    /// - **mtime-based staleness:** a lock older than 30 minutes is treated as
    ///   stale and stolen. A writer that hangs for >30 minutes (without exiting)
    ///   can be stolen from, since there is no PID-liveness probe (that would
    ///   require libc and is out of scope for v1).
    /// - **no release on SIGKILL:** if the holding process is killed with
    ///   SIGKILL, the lock file is not cleaned up and the next writer must wait
    ///   out the 30-minute staleness window (or the user deletes the file, as
    ///   the suggestedFix advises).
    pub fn lock(&self) -> Result<StoreLock, LensError> {
        let path = self.lock_path();
        match create_lock_file(&path) {
            Ok(()) => Ok(StoreLock { path }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&path)? {
                    eprintln!("warning: stealing stale index lock {}", path.display());
                    let _ = fs::remove_file(&path);
                    // F6: a concurrent stealer that loses the race hits
                    // AlreadyExists here. Map it to the same "index is already
                    // locked" error with the same suggestedFix as the young-lock
                    // path — the other stealer won legitimately.
                    create_lock_file(&path).map_err(|err| {
                        if err.kind() == std::io::ErrorKind::AlreadyExists {
                            lock_busy_error(&path)
                        } else {
                            lock_error(&path, err)
                        }
                    })?;
                    Ok(StoreLock { path })
                } else {
                    Err(lock_busy_error(&path))
                }
            }
            Err(err) => Err(lock_error(&path, err)),
        }
    }

    pub fn ensure_meta(&self, library_path: &Path, model: &str) -> Result<(), LensError> {
        let now = rfc3339_now();
        if self.meta_path().exists() {
            let mut meta = self.read_meta()?;
            meta.library_path = fs::canonicalize(library_path)
                .unwrap_or_else(|_| library_path.to_path_buf())
                .to_string_lossy()
                .to_string();
            meta.model = model.to_string();
            meta.schema_version = SCHEMA_VERSION;
            meta.prompt_version = PROMPT_VERSION;
            meta.normalizer_version = NORMALIZER_VERSION;
            meta.updated_at = now;
            write_json_atomic(&self.meta_path(), &meta)
        } else {
            let meta = StoreMeta {
                library_path: fs::canonicalize(library_path)
                    .unwrap_or_else(|_| library_path.to_path_buf())
                    .to_string_lossy()
                    .to_string(),
                model: model.to_string(),
                schema_version: SCHEMA_VERSION,
                prompt_version: PROMPT_VERSION,
                normalizer_version: NORMALIZER_VERSION,
                created_at: now.clone(),
                updated_at: now,
            };
            write_json_atomic(&self.meta_path(), &meta)
        }
    }

    pub fn load(&self, model: &str) -> Result<LoadedIndex, LensError> {
        let stale_all = match self.read_meta_optional()? {
            Some(meta) => {
                if meta.schema_version > SCHEMA_VERSION {
                    return Err(LensError::config(format!(
                        "index schema version {} is newer than supported version {SCHEMA_VERSION}",
                        meta.schema_version
                    ))
                    .with_suggested_fix("upgrade lens or reindex the library"));
                }
                meta.model != model
                    || meta.prompt_version != PROMPT_VERSION
                    || meta.normalizer_version != NORMALIZER_VERSION
            }
            None => false,
        };

        let mut warnings = Vec::new();
        let path = self.index_path();
        if !path.exists() {
            return Ok(LoadedIndex {
                records: Vec::new(),
                stale_all,
                warnings,
            });
        }

        let file = File::open(&path).map_err(|err| {
            LensError::config(format!("failed to read index {}: {err}", path.display()))
        })?;
        let lines = BufReader::new(file)
            .lines()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| {
                LensError::config(format!("failed to read index {}: {err}", path.display()))
            })?;

        let mut records = Vec::with_capacity(lines.len());
        let final_index = lines.len().saturating_sub(1);
        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<IndexRecord>(line) {
                Ok(record) => records.push(record),
                Err(err) if i == final_index => {
                    warnings.push(format!(
                        "dropped torn final index line in {}: {err}",
                        path.display()
                    ));
                }
                Err(err) => {
                    return Err(LensError::config(format!(
                        "corrupt index line {} in {}: {err}",
                        i + 1,
                        path.display()
                    ))
                    .with_suggested_fix("reindex"));
                }
            }
        }

        // F13: dedup last-wins by rel_path so a mid-run crash that leaves
        // duplicate JSONL rows doesn't confuse resume. Preserve last
        // occurrence, stable order by first occurrence (same semantics as
        // rewrite_last_wins).
        let records = dedup_last_wins(records);

        Ok(LoadedIndex {
            records,
            stale_all,
            warnings,
        })
    }

    pub fn append(&self, record: &IndexRecord) -> Result<(), LensError> {
        let path = self.index_path();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| {
                LensError::config(format!("failed to open index {}: {err}", path.display()))
            })?;
        let mut line = serde_json::to_vec(record)
            .map_err(|err| LensError::config(format!("failed to serialize index record: {err}")))?;
        line.push(b'\n');
        let written = file.write(&line).map_err(|err| {
            LensError::config(format!("failed to append index {}: {err}", path.display()))
        })?;
        if written != line.len() {
            return Err(LensError::config(format!(
                "failed to append complete index line to {}: {}",
                path.display(),
                Error::new(ErrorKind::WriteZero, "short append write")
            )));
        }
        file.flush().map_err(|err| {
            LensError::config(format!("failed to flush index {}: {err}", path.display()))
        })
    }

    pub fn rewrite_last_wins<I>(&self, records: I) -> Result<usize, LensError>
    where
        I: IntoIterator<Item = IndexRecord>,
    {
        let mut ordered = Vec::new();
        let mut positions: HashMap<String, usize> = HashMap::new();
        for record in records {
            if let Some(pos) = positions.get(&record.rel_path).copied() {
                ordered[pos] = record;
            } else {
                positions.insert(record.rel_path.clone(), ordered.len());
                ordered.push(record);
            }
        }

        let tmp = self.dir.join("index.jsonl.tmp");
        let mut file = File::create(&tmp).map_err(|err| {
            LensError::config(format!("failed to create {}: {err}", tmp.display()))
        })?;
        for record in &ordered {
            serde_json::to_writer(&mut file, record).map_err(|err| {
                LensError::config(format!("failed to serialize index record: {err}"))
            })?;
            file.write_all(b"\n").map_err(|err| {
                LensError::config(format!("failed to write {}: {err}", tmp.display()))
            })?;
        }
        file.flush().map_err(|err| {
            LensError::config(format!("failed to flush {}: {err}", tmp.display()))
        })?;
        // F8: fsync the tmp file before rename so the data is durable on disk.
        file.sync_all()
            .map_err(|err| LensError::config(format!("failed to sync {}: {err}", tmp.display())))?;
        fs::rename(&tmp, self.index_path()).map_err(|err| {
            LensError::config(format!(
                "failed to replace {}: {err}",
                self.index_path().display()
            ))
        })?;
        Ok(ordered.len())
    }

    fn read_meta(&self) -> Result<StoreMeta, LensError> {
        let path = self.meta_path();
        let text = fs::read_to_string(&path).map_err(|err| {
            LensError::config(format!("failed to read metadata {}: {err}", path.display()))
        })?;
        serde_json::from_str(&text).map_err(|err| {
            LensError::config(format!(
                "failed to parse metadata {}: {err}",
                path.display()
            ))
            .with_suggested_fix("reindex")
        })
    }

    fn read_meta_optional(&self) -> Result<Option<StoreMeta>, LensError> {
        if self.meta_path().exists() {
            self.read_meta().map(Some)
        } else {
            Ok(None)
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn data_home() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn hex_prefix(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let hash = hasher.finalize();
    hash[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), LensError> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| LensError::config(format!("failed to serialize metadata: {err}")))?;
    // F8: use an explicit File so we can fsync before the atomic rename.
    let mut file = File::create(&tmp)
        .map_err(|err| LensError::config(format!("failed to create {}: {err}", tmp.display())))?;
    file.write_all(&bytes)
        .map_err(|err| LensError::config(format!("failed to write {}: {err}", tmp.display())))?;
    file.flush()
        .map_err(|err| LensError::config(format!("failed to flush {}: {err}", tmp.display())))?;
    file.sync_all()
        .map_err(|err| LensError::config(format!("failed to sync {}: {err}", tmp.display())))?;
    fs::rename(&tmp, path)
        .map_err(|err| LensError::config(format!("failed to replace {}: {err}", path.display())))
}

fn create_lock_file(path: &Path) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let body = serde_json::json!({
        "pid": std::process::id(),
        "startedAt": rfc3339_now(),
    });
    writeln!(file, "{body}")?;
    file.flush()
}

fn lock_error(path: &Path, err: std::io::Error) -> LensError {
    LensError::config(format!("failed to create lock {}: {err}", path.display()))
}

fn lock_busy_error(path: &Path) -> LensError {
    LensError::config(format!("index is already locked at {}", path.display())).with_suggested_fix(
        format!(
            "wait for the other lens index run, or delete {} if that process is dead",
            path.display()
        ),
    )
}

/// Deduplicates records last-wins by `rel_path`: when the same rel_path
/// appears multiple times, the last occurrence's values win, but the record
/// stays at the position of the first occurrence (stable order). Mirrors the
/// semantics of `rewrite_last_wins` so a load after a mid-run crash yields the
/// same deduplicated set.
fn dedup_last_wins(records: Vec<IndexRecord>) -> Vec<IndexRecord> {
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut ordered = Vec::with_capacity(records.len());
    for record in records {
        if let Some(pos) = positions.get(&record.rel_path).copied() {
            ordered[pos] = record;
        } else {
            positions.insert(record.rel_path.clone(), ordered.len());
            ordered.push(record);
        }
    }
    ordered
}

fn lock_is_stale(path: &Path) -> Result<bool, LensError> {
    let meta = fs::metadata(path).map_err(|err| {
        LensError::config(format!("failed to stat lock {}: {err}", path.display()))
    })?;
    let age = SystemTime::now()
        .duration_since(meta.modified().unwrap_or(SystemTime::now()))
        .unwrap_or_default();
    Ok(age > STALE_LOCK_AFTER)
}

fn rfc3339_now() -> String {
    system_time_to_rfc3339(SystemTime::now())
}

fn system_time_to_rfc3339(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (year, month, day, hour, minute, second) = unix_to_utc(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn unix_to_utc(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let second_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (second_of_day / 3_600) as u32,
        ((second_of_day % 3_600) / 60) as u32,
        (second_of_day % 60) as u32,
    )
}

// Howard Hinnant's public-domain civil date conversion.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719_468;
    let era = (if days >= 0 { days } else { days - 146_096 }) / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(rel_path: &str, size: u64, description: &str) -> IndexRecord {
        IndexRecord {
            rel_path: rel_path.to_string(),
            size,
            mtime_ns: 1,
            description: description.to_string(),
            filename: "name".to_string(),
            tags: vec!["tag".to_string()],
            text_content: String::new(),
            kind: "photo".to_string(),
        }
    }

    #[test]
    fn append_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();

        store.append(&record("a.jpg", 1, "one")).unwrap();
        store.append(&record("b.jpg", 2, "two")).unwrap();

        let loaded = store.load("m").unwrap();
        assert_eq!(loaded.records.len(), 2);
        assert_eq!(loaded.records[0].rel_path, "a.jpg");
        assert_eq!(loaded.records[1].description, "two");
    }

    #[test]
    fn torn_final_line_is_dropped_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();
        store.append(&record("a.jpg", 1, "one")).unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.index_path())
            .unwrap();
        file.write_all(b"{not-json").unwrap();

        let loaded = store.load("m").unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.warnings.len(), 1);
    }

    #[test]
    fn corrupt_middle_line_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();
        store.append(&record("a.jpg", 1, "one")).unwrap();
        fs::write(
            store.index_path(),
            format!(
                "{}\nnot json\n{}\n",
                serde_json::to_string(&record("a.jpg", 1, "one")).unwrap(),
                serde_json::to_string(&record("b.jpg", 2, "two")).unwrap()
            ),
        )
        .unwrap();

        let err = store.load("m").unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert_eq!(err.suggested_fix(), Some("reindex"));
    }

    #[test]
    fn prune_rewrite_keeps_last_wins_per_rel_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();

        store
            .rewrite_last_wins(vec![
                record("a.jpg", 1, "old"),
                record("b.jpg", 1, "b"),
                record("a.jpg", 2, "new"),
            ])
            .unwrap();

        let loaded = store.load("m").unwrap();
        assert_eq!(loaded.records.len(), 2);
        assert_eq!(loaded.records[0].description, "new");
        assert_eq!(loaded.records[1].rel_path, "b.jpg");
    }

    #[test]
    fn lock_conflict_young_errors_with_fix() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();
        let _lock = store.lock().unwrap();

        let err = store.lock().unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(err.suggested_fix().unwrap().contains("index.lock"));
    }

    #[test]
    fn stale_lock_is_stolen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();
        fs::write(store.lock_path(), "old").unwrap();
        let old = File::open(store.lock_path()).unwrap();
        let stale = std::fs::FileTimes::new()
            .set_modified(SystemTime::now() - Duration::from_secs(31 * 60));
        old.set_times(stale).unwrap();
        drop(old);

        let _lock = store.lock().unwrap();
    }

    #[test]
    fn meta_version_gates_and_stale_all() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();
        let now = rfc3339_now();
        let meta = StoreMeta {
            library_path: "/x".into(),
            model: "old-model".into(),
            schema_version: 1,
            prompt_version: 1,
            normalizer_version: 1,
            created_at: now.clone(),
            updated_at: now,
        };
        write_json_atomic(&store.meta_path(), &meta).unwrap();
        assert!(store.load("new-model").unwrap().stale_all);

        let mut newer = meta;
        newer.schema_version = 2;
        write_json_atomic(&store.meta_path(), &newer).unwrap();
        let err = store.load("new-model").unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(err.suggested_fix().unwrap().contains("upgrade"));
    }

    #[test]
    fn f13_load_dedupes_duplicate_rel_paths_last_wins() {
        // A mid-run crash can leave duplicate JSONL rows for the same rel_path.
        // load() must dedup last-wins so resume sees one record with the later
        // values, in stable order by first occurrence.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();

        // Hand-write an index file with a duplicate rel_path.
        let lines = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&record("a.jpg", 1, "first")).unwrap(),
            serde_json::to_string(&record("b.jpg", 2, "only")).unwrap(),
            serde_json::to_string(&record("a.jpg", 3, "second")).unwrap(),
        );
        fs::write(store.index_path(), lines).unwrap();

        let loaded = store.load("m").unwrap();
        assert_eq!(loaded.records.len(), 2);
        // "a.jpg" keeps the later values but stays at first-occurrence position.
        assert_eq!(loaded.records[0].rel_path, "a.jpg");
        assert_eq!(loaded.records[0].description, "second");
        assert_eq!(loaded.records[0].size, 3);
        assert_eq!(loaded.records[1].rel_path, "b.jpg");
    }
}
