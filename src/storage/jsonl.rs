use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::HashSet;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// A JSONL line that could not be deserialized and was therefore skipped.
///
/// Records are skipped rather than fatal so that a single unreadable line — a
/// record written by a newer rite, or a truncated write — cannot deny access to
/// every other record in the file. Skips are never silent: readers warn (see
/// [`report_skipped`]) and `rite doctor` re-scans and reports the totals.
#[derive(Debug, Clone)]
pub struct SkippedLine {
    /// File the line came from.
    pub path: PathBuf,
    /// 1-based line number, when the read started at the top of the file.
    /// `None` for reads that begin at a mid-file byte offset.
    pub line: Option<u64>,
    /// Byte offset of the first byte of the skipped line.
    pub byte_offset: u64,
    /// Why the line could not be parsed.
    pub error: String,
}

impl fmt::Display for SkippedLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{}: {}", self.path.display(), line, self.error),
            None => write!(
                f,
                "{}@byte {}: {}",
                self.path.display(),
                self.byte_offset,
                self.error
            ),
        }
    }
}

/// Files already warned about in this process, so a repeated read (a TUI
/// refresh loop, say) does not spam stderr with the same diagnostic.
fn warned_paths() -> &'static Mutex<HashSet<PathBuf>> {
    static WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    WARNED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Make skipped lines observable.
///
/// Every skip is emitted as a `tracing` warning (structured, picked up by
/// telemetry when configured). Additionally, the first skip per file per
/// process prints a human-visible note to stderr, because tracing is a no-op
/// unless telemetry is enabled and silent data loss is worse than noise.
fn report_skipped(skipped: &[SkippedLine]) {
    if skipped.is_empty() {
        return;
    }

    for skip in skipped {
        tracing::warn!(
            path = %skip.path.display(),
            line = skip.line,
            byte_offset = skip.byte_offset,
            error = %skip.error,
            "skipping unreadable JSONL record"
        );
    }

    let path = skipped[0].path.clone();
    let first_time = warned_paths()
        .lock()
        .map(|mut seen| seen.insert(path.clone()))
        .unwrap_or(false);

    if first_time {
        eprintln!(
            "warning: skipped {} unreadable line(s) in {} (first: {}); run `rite doctor` for details",
            skipped.len(),
            path.display(),
            skipped[0]
        );
    }
}

/// Try to deserialize one raw JSONL line.
///
/// Returns `Ok(None)` for blank lines. A line that cannot be parsed is recorded
/// in `skipped` (with file/line/offset context) and yields `Ok(None)` too, so
/// callers continue with the rest of the file.
fn parse_line<T: DeserializeOwned>(
    raw: &[u8],
    path: &Path,
    line: Option<u64>,
    byte_offset: u64,
    skipped: &mut Vec<SkippedLine>,
) -> Option<T> {
    let text = match std::str::from_utf8(raw) {
        Ok(text) => text,
        Err(error) => {
            skipped.push(SkippedLine {
                path: path.to_path_buf(),
                line,
                byte_offset,
                error: format!("invalid UTF-8: {}", error),
            });
            return None;
        }
    };

    if text.trim().is_empty() {
        return None;
    }

    match serde_json::from_str(text) {
        Ok(record) => Some(record),
        Err(error) => {
            skipped.push(SkippedLine {
                path: path.to_path_buf(),
                line,
                byte_offset,
                error: error.to_string(),
            });
            None
        }
    }
}

/// Append a single record to a JSONL file with exclusive locking.
///
/// This ensures safe concurrent writes from multiple processes.
pub fn append_record<T: Serialize>(path: &Path, record: &T) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open file for append: {}", path.display()))?;

    // Acquire exclusive lock (blocks until available)
    file.lock_exclusive()
        .with_context(|| format!("Failed to acquire lock on: {}", path.display()))?;

    // Serialize and write the record
    let json = serde_json::to_string(record).with_context(|| "Failed to serialize record")?;

    let mut writer = std::io::BufWriter::new(&file);
    writeln!(writer, "{}", json)
        .with_context(|| format!("Failed to write to: {}", path.display()))?;

    writer.flush()?;

    // Ensure data is written to disk
    file.sync_all()
        .with_context(|| format!("Failed to sync: {}", path.display()))?;

    // Lock is released when file is dropped
    Ok(())
}

/// Append multiple records to a JSONL file with exclusive locking.
///
/// More efficient than calling `append_record` multiple times.
pub fn append_records<T: Serialize>(path: &Path, records: &[T]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open file for append: {}", path.display()))?;

    file.lock_exclusive()
        .with_context(|| format!("Failed to acquire lock on: {}", path.display()))?;

    let mut writer = std::io::BufWriter::new(&file);

    for record in records {
        let json = serde_json::to_string(record).with_context(|| "Failed to serialize record")?;
        writeln!(writer, "{}", json)
            .with_context(|| format!("Failed to write to: {}", path.display()))?;
    }

    writer.flush()?;
    file.sync_all()?;

    Ok(())
}

/// Atomically check a condition and append a record if the condition is met.
///
/// This function:
/// 1. Acquires an exclusive lock on the file
/// 2. Reads all existing records
/// 3. Calls the predicate function with the records
/// 4. If the predicate returns true, appends the new record
/// 5. Returns whether the append happened
///
/// This is useful for implementing compare-and-swap style operations.
pub fn append_if<T, F>(path: &Path, record: &T, predicate: F) -> Result<bool>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce(&[T]) -> bool,
{
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open file: {}", path.display()))?;

    // Acquire exclusive lock for atomic read-check-write
    file.lock_exclusive()
        .with_context(|| format!("Failed to acquire lock on: {}", path.display()))?;

    // Read existing records while holding the lock. Unreadable lines are
    // skipped (and reported) rather than aborting the check-and-append.
    let mut reader = BufReader::new(&file);
    let mut records: Vec<T> = Vec::new();
    let mut skipped = Vec::new();

    let mut raw = Vec::new();
    let mut byte_offset = 0u64;
    let mut line_no = 0u64;

    loop {
        raw.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut raw)
            .with_context(|| format!("Failed to read from: {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }

        let line_start = byte_offset;
        byte_offset += bytes_read as u64;
        line_no += 1;

        if let Some(rec) = parse_line(&raw, path, Some(line_no), line_start, &mut skipped) {
            records.push(rec);
        }
    }

    report_skipped(&skipped);

    // Check if we should append
    if !predicate(&records) {
        // Lock is released when file is dropped
        return Ok(false);
    }

    // Append the record
    let json = serde_json::to_string(record).with_context(|| "Failed to serialize record")?;

    let mut writer = std::io::BufWriter::new(&file);
    writeln!(writer, "{}", json)
        .with_context(|| format!("Failed to write to: {}", path.display()))?;

    writer.flush()?;
    file.sync_all()?;

    Ok(true)
}

/// Read all records from a JSONL file.
///
/// Returns an empty Vec if the file doesn't exist.
///
/// Lines that cannot be deserialized (records from a newer rite, truncated
/// writes, non-UTF-8 bytes) are skipped and reported rather than failing the
/// whole read — one bad line must not deny access to the rest of the file.
pub fn read_records<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let (records, skipped) = read_records_reporting(path)?;
    report_skipped(&skipped);
    Ok(records)
}

/// Like [`read_records`], but returns the skipped lines instead of reporting
/// them, so callers (notably `rite doctor`) can surface the details.
pub fn read_records_reporting<T: DeserializeOwned>(
    path: &Path,
) -> Result<(Vec<T>, Vec<SkippedLine>)> {
    let mut records = Vec::new();
    let skipped = scan_records(path, Some(&mut records))?;
    Ok((records, skipped))
}

/// Parse every line of a JSONL file as `T` and return only the lines that
/// failed. Nothing is retained, so this is cheap enough to run over every file
/// in the data directory (used by `rite doctor`).
pub fn scan_skipped<T: DeserializeOwned>(path: &Path) -> Result<Vec<SkippedLine>> {
    scan_records::<T>(path, None)
}

/// Shared implementation of the whole-file read.
///
/// When `records` is `Some`, successfully parsed records are collected into it;
/// when `None` they are parsed and dropped. Either way, unparsable lines are
/// returned with their file/line/offset context.
fn scan_records<T: DeserializeOwned>(
    path: &Path,
    mut records: Option<&mut Vec<T>>,
) -> Result<Vec<SkippedLine>> {
    let mut skipped = Vec::new();

    if !path.exists() {
        return Ok(skipped);
    }

    let file =
        File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;

    // Use shared lock for reading
    file.lock_shared()
        .with_context(|| format!("Failed to acquire shared lock on: {}", path.display()))?;

    let mut reader = BufReader::new(&file);
    let mut raw = Vec::new();
    let mut byte_offset = 0u64;
    let mut line_no = 0u64;

    loop {
        raw.clear();
        let bytes_read = reader.read_until(b'\n', &mut raw).with_context(|| {
            format!(
                "Failed to read line {} from: {}",
                line_no + 1,
                path.display()
            )
        })?;
        if bytes_read == 0 {
            break;
        }

        let line_start = byte_offset;
        byte_offset += bytes_read as u64;
        line_no += 1;

        if let Some(record) = parse_line::<T>(&raw, path, Some(line_no), line_start, &mut skipped)
            && let Some(records) = records.as_mut()
        {
            records.push(record);
        }
    }

    Ok(skipped)
}

/// Read records from a JSONL file starting at a byte offset.
///
/// Returns the records and the new byte offset after reading.
/// Useful for incremental reading (e.g., tailing a file).
pub fn read_records_from_offset<T: DeserializeOwned>(
    path: &Path,
    offset: u64,
) -> Result<(Vec<T>, u64)> {
    read_records_from_offset_limited(path, offset, None)
}

/// Read up to `limit` records from a JSONL file starting at a byte offset.
///
/// Returns the records and the byte offset immediately after the last line
/// consumed. If the limit is reached before EOF, the returned offset can be
/// used to continue without skipping unread records.
pub fn read_records_from_offset_limited<T: DeserializeOwned>(
    path: &Path,
    offset: u64,
    limit: Option<usize>,
) -> Result<(Vec<T>, u64)> {
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }

    if limit == Some(0) {
        return Ok((Vec::new(), offset));
    }

    let mut file =
        File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;

    file.lock_shared()
        .with_context(|| format!("Failed to acquire shared lock on: {}", path.display()))?;

    // Seek to offset
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("Failed to seek in: {}", path.display()))?;

    let mut reader = BufReader::new(&file);
    let mut records = Vec::new();
    let mut skipped = Vec::new();
    let mut new_offset = offset;
    let mut raw = Vec::new();

    loop {
        raw.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut raw)
            .with_context(|| format!("Failed to read from: {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }

        let line_start = new_offset;
        new_offset += bytes_read as u64;

        // Absolute line numbers are unknown when starting mid-file, so the
        // byte offset carries the position context here.
        if let Some(record) = parse_line::<T>(&raw, path, None, line_start, &mut skipped) {
            records.push(record);

            if limit.is_some_and(|limit| records.len() >= limit) {
                break;
            }
        }
    }

    report_skipped(&skipped);

    if limit.is_none() {
        // Get the new offset while still holding the shared lock. Reopening
        // after reading would leave a race where a concurrent append could be
        // included in the returned offset without its records being returned.
        new_offset = reader.seek(SeekFrom::End(0))?;
    }

    Ok((records, new_offset))
}

/// Count the number of records in a JSONL file.
pub fn count_records(path: &Path) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }

    let file =
        File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;

    file.lock_shared()?;

    let reader = BufReader::new(&file);
    let count = reader
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .count();

    Ok(count)
}

/// Read the last N records from a JSONL file.
///
/// This reads the entire file but only returns the last N records.
/// For very large files, consider using offset-based reading instead.
pub fn read_last_n<T: DeserializeOwned>(path: &Path, n: usize) -> Result<Vec<T>> {
    let all_records: Vec<T> = read_records(path)?;
    let start = all_records.len().saturating_sub(n);
    Ok(all_records.into_iter().skip(start).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestRecord {
        id: u32,
        name: String,
    }

    #[test]
    fn test_append_and_read() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");

        let record1 = TestRecord {
            id: 1,
            name: "Alice".to_string(),
        };
        let record2 = TestRecord {
            id: 2,
            name: "Bob".to_string(),
        };

        append_record(&path, &record1).unwrap();
        append_record(&path, &record2).unwrap();

        let records: Vec<TestRecord> = read_records(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], record1);
        assert_eq!(records[1], record2);
    }

    #[test]
    fn test_append_records_batch() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");

        let records = vec![
            TestRecord {
                id: 1,
                name: "One".to_string(),
            },
            TestRecord {
                id: 2,
                name: "Two".to_string(),
            },
            TestRecord {
                id: 3,
                name: "Three".to_string(),
            },
        ];

        append_records(&path, &records).unwrap();

        let read: Vec<TestRecord> = read_records(&path).unwrap();
        assert_eq!(read, records);
    }

    #[test]
    fn test_read_nonexistent_file() {
        let path = Path::new("/nonexistent/path/file.jsonl");
        let records: Vec<TestRecord> = read_records(path).unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn test_read_from_offset() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");

        let record1 = TestRecord {
            id: 1,
            name: "First".to_string(),
        };
        append_record(&path, &record1).unwrap();

        // Get current offset
        let (_, offset) = read_records_from_offset::<TestRecord>(&path, 0).unwrap();

        // Add more records
        let record2 = TestRecord {
            id: 2,
            name: "Second".to_string(),
        };
        let record3 = TestRecord {
            id: 3,
            name: "Third".to_string(),
        };
        append_record(&path, &record2).unwrap();
        append_record(&path, &record3).unwrap();

        // Read from offset - should only get new records
        let (new_records, _) = read_records_from_offset::<TestRecord>(&path, offset).unwrap();
        assert_eq!(new_records.len(), 2);
        assert_eq!(new_records[0], record2);
        assert_eq!(new_records[1], record3);
    }

    #[test]
    fn test_read_from_offset_limited_returns_continuation_offset() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");

        let records: Vec<TestRecord> = (1..=3)
            .map(|id| TestRecord {
                id,
                name: format!("Record{}", id),
            })
            .collect();
        append_records(&path, &records).unwrap();

        let (first, next_offset) =
            read_records_from_offset_limited::<TestRecord>(&path, 0, Some(1)).unwrap();
        assert_eq!(first, vec![records[0].clone()]);
        assert!(next_offset > 0);
        assert!(next_offset < std::fs::metadata(&path).unwrap().len());

        let (remaining, final_offset) =
            read_records_from_offset_limited::<TestRecord>(&path, next_offset, None).unwrap();
        assert_eq!(remaining, records[1..].to_vec());
        assert_eq!(final_offset, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn test_count_records() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");

        assert_eq!(count_records(&path).unwrap(), 0);

        let records = vec![
            TestRecord {
                id: 1,
                name: "One".to_string(),
            },
            TestRecord {
                id: 2,
                name: "Two".to_string(),
            },
        ];
        append_records(&path, &records).unwrap();

        assert_eq!(count_records(&path).unwrap(), 2);
    }

    /// Write records with an unreadable line wedged in the middle.
    fn file_with_bad_line(path: &Path) {
        use std::io::Write;

        append_record(
            path,
            &TestRecord {
                id: 1,
                name: "first".to_string(),
            },
        )
        .unwrap();

        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        // Shape of a record a newer rite might write: right type name, wrong shape.
        writeln!(file, r#"{{"id":"not-a-number","name":"future"}}"#).unwrap();
        drop(file);

        append_record(
            path,
            &TestRecord {
                id: 3,
                name: "third".to_string(),
            },
        )
        .unwrap();
    }

    #[test]
    fn test_read_records_skips_unparsable_line() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");
        file_with_bad_line(&path);

        let records: Vec<TestRecord> = read_records(&path).unwrap();
        assert_eq!(records.len(), 2, "one bad line must not lose the good ones");
        assert_eq!(records[0].id, 1);
        assert_eq!(records[1].id, 3);
    }

    #[test]
    fn test_read_records_reporting_keeps_line_context() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");
        file_with_bad_line(&path);

        let (records, skipped) = read_records_reporting::<TestRecord>(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].path, path);
        assert_eq!(skipped[0].line, Some(2));
        assert!(skipped[0].byte_offset > 0);
        assert!(!skipped[0].error.is_empty());
        // Display carries both the file and the position.
        let rendered = skipped[0].to_string();
        assert!(rendered.contains(&path.display().to_string()));
        assert!(rendered.contains(":2:"));
    }

    #[test]
    fn test_scan_skipped_counts_without_collecting() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");
        file_with_bad_line(&path);

        let skipped = scan_skipped::<TestRecord>(&path).unwrap();
        assert_eq!(skipped.len(), 1);

        // A clean file reports nothing, and a missing file is not an error.
        let clean = temp.path().join("clean.jsonl");
        append_record(
            &clean,
            &TestRecord {
                id: 1,
                name: "ok".to_string(),
            },
        )
        .unwrap();
        assert!(scan_skipped::<TestRecord>(&clean).unwrap().is_empty());
        assert!(
            scan_skipped::<TestRecord>(&temp.path().join("missing.jsonl"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_read_from_offset_skips_unparsable_line() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");
        file_with_bad_line(&path);

        let (records, offset) = read_records_from_offset::<TestRecord>(&path, 0).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(offset, std::fs::metadata(&path).unwrap().len());

        // Limits count parsed records, and the cursor stays usable across a skip.
        let (first, next) =
            read_records_from_offset_limited::<TestRecord>(&path, 0, Some(1)).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, 1);
        let (rest, _) =
            read_records_from_offset_limited::<TestRecord>(&path, next, Some(1)).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, 3, "the skipped line must not stall the cursor");
    }

    #[test]
    fn test_append_if_tolerates_unparsable_line() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");
        file_with_bad_line(&path);

        let new = TestRecord {
            id: 4,
            name: "fourth".to_string(),
        };
        let appended =
            append_if(&path, &new, |existing: &[TestRecord]| existing.len() == 2).unwrap();
        assert!(appended);

        let records: Vec<TestRecord> = read_records(&path).unwrap();
        assert_eq!(records.len(), 3);
    }

    #[test]
    fn test_non_utf8_line_is_skipped() {
        use std::io::Write;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");

        {
            let mut file = File::create(&path).unwrap();
            file.write_all(&[0xff, 0xfe, b'\n']).unwrap();
        }
        append_record(
            &path,
            &TestRecord {
                id: 7,
                name: "after".to_string(),
            },
        )
        .unwrap();

        let (records, skipped) = read_records_reporting::<TestRecord>(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, 7);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].error.contains("UTF-8"));
    }

    #[test]
    fn test_read_last_n() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("test.jsonl");

        let records: Vec<TestRecord> = (1..=10)
            .map(|i| TestRecord {
                id: i,
                name: format!("Record{}", i),
            })
            .collect();

        append_records(&path, &records).unwrap();

        let last3: Vec<TestRecord> = read_last_n(&path, 3).unwrap();
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[0].id, 8);
        assert_eq!(last3[1].id, 9);
        assert_eq!(last3[2].id, 10);
    }
}
