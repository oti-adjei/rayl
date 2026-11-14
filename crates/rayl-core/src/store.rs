use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Tables `search` and `read` cannot be built without.
pub const REQUIRED_TABLES: &[&str] = &["messages", "subjects", "mailboxes"];

/// Tables we expect but can degrade without (richer filters, threading).
pub const EXPECTED_TABLES: &[&str] = &["addresses", "recipients", "attachments"];

/// Tables whose shape the search and read paths are built against.
pub const CORE_TABLES: &[&str] =
    &["messages", "subjects", "addresses", "recipients", "mailboxes", "attachments"];

/// `~/Library/Mail`
pub fn mail_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or(Error::NoHome)?;
    Ok(PathBuf::from(home).join("Library").join("Mail"))
}

/// A versioned Apple Mail store, e.g. `~/Library/Mail/V10`.
#[derive(Debug, Clone)]
pub struct MailStore {
    pub root: PathBuf,
    pub version: u32,
    pub path: PathBuf,
}

impl MailStore {
    pub fn discover() -> Result<Self> {
        Self::discover_in(&mail_root()?)
    }

    /// Picks the highest `V<n>` directory. Mail leaves older ones behind after an upgrade,
    /// and the highest is the live one.
    pub fn discover_in(root: &Path) -> Result<Self> {
        let entries = fs::read_dir(root).map_err(|e| Error::from_io(root, e))?;

        let mut best: Option<(u32, PathBuf)> = None;
        for entry in entries {
            let entry = entry.map_err(|e| Error::from_io(root, e))?;
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name();
            let Some(version) = name.to_str().and_then(parse_version_dir) else {
                continue;
            };
            if best.as_ref().is_none_or(|(seen, _)| version > *seen) {
                best = Some((version, entry.path()));
            }
        }

        let (version, path) = best.ok_or_else(|| Error::NoVersionedStore { path: root.into() })?;
        Ok(Self { root: root.into(), version, path })
    }

    pub fn envelope_index(&self) -> PathBuf {
        self.path.join("MailData").join("Envelope Index")
    }

    /// Copies the index (and any WAL sidecars) somewhere we may safely open read-write, so
    /// SQLite can replay the WAL without us ever touching Mail's own files.
    pub fn snapshot_envelope_index(&self) -> Result<IndexSnapshot> {
        let source = self.envelope_index();
        if !source.try_exists().map_err(|e| Error::from_io(&source, e))? {
            return Err(Error::EnvelopeIndexMissing { path: source });
        }

        let dir = tempfile::tempdir().map_err(|e| Error::Io {
            path: std::env::temp_dir(),
            source: e,
        })?;
        let db = dir.path().join("Envelope Index");
        fs::copy(&source, &db).map_err(|e| Error::from_io(&source, e))?;

        let mut sidecars = Vec::new();
        for suffix in ["-wal", "-shm"] {
            let from = with_suffix(&source, suffix);
            if from.try_exists().unwrap_or(false) {
                let to = with_suffix(&db, suffix);
                fs::copy(&from, &to).map_err(|e| Error::from_io(&from, e))?;
                sidecars.push(suffix.to_string());
            }
        }

        let bytes = fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
        Ok(IndexSnapshot { _dir: dir, db, sidecars, bytes })
    }

    /// Counts `.emlx` bodies, bounded so a large mailbox cannot stall the probe.
    pub fn scan_emlx(&self, cap: usize) -> Result<EmlxScan> {
        let mut scan = EmlxScan::default();
        let mut stack = vec![self.path.clone()];

        while let Some(dir) = stack.pop() {
            if scan.complete_count + scan.partial_count >= cap {
                scan.capped = true;
                break;
            }
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                // A single unreadable subdirectory should not abort the whole scan.
                Err(_) => {
                    scan.unreadable_dirs += 1;
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(path);
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.ends_with(".partial.emlx") {
                        scan.partial_count += 1;
                    } else if name.ends_with(".emlx") {
                        scan.complete_count += 1;
                    }
                }
            }
        }

        Ok(scan)
    }
}

/// A throwaway copy of the envelope index. Deleted when dropped.
#[derive(Debug)]
pub struct IndexSnapshot {
    _dir: tempfile::TempDir,
    pub db: PathBuf,
    pub sidecars: Vec<String>,
    pub bytes: u64,
}

#[derive(Debug, Default, Clone)]
pub struct EmlxScan {
    pub complete_count: usize,
    /// Headers-only placeholders: the account has not downloaded these bodies.
    pub partial_count: usize,
    pub unreadable_dirs: usize,
    pub capped: bool,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub decl_type: String,
    pub notnull: bool,
    pub pk: bool,
}

#[derive(Debug, Clone)]
pub struct TableShape {
    pub name: String,
    pub columns: Vec<Column>,
    pub rows: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct SchemaReport {
    pub tables: Vec<String>,
    pub message_columns: Vec<String>,
    pub message_count: Option<i64>,
    pub missing_required: Vec<String>,
    pub missing_expected: Vec<String>,
}

impl SchemaReport {
    pub fn is_usable(&self) -> bool {
        self.missing_required.is_empty()
    }
}

/// Reads the shape of a snapshotted index. Deliberately descriptive rather than strict: the
/// schema is undocumented and shifts between macOS releases, so we report what is there and
/// only hard-fail on the handful of tables search and read genuinely need.
pub fn inspect_schema(db: &Path) -> Result<SchemaReport> {
    let conn = rusqlite::Connection::open(db)?;

    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let tables = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let missing = |wanted: &[&str]| -> Vec<String> {
        wanted
            .iter()
            .filter(|t| !tables.iter().any(|have| have == *t))
            .map(|t| t.to_string())
            .collect()
    };
    let missing_required = missing(REQUIRED_TABLES);
    let missing_expected = missing(EXPECTED_TABLES);

    let (message_columns, message_count) = if tables.iter().any(|t| t == "messages") {
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('messages') ORDER BY cid")?;
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let count = conn
            .query_row("SELECT count(*) FROM messages", [], |row| row.get::<_, i64>(0))
            .ok();
        (columns, count)
    } else {
        (Vec::new(), None)
    };

    Ok(SchemaReport {
        tables,
        message_columns,
        message_count,
        missing_required,
        missing_expected,
    })
}

/// Describes one table: column names, declared types, and a row count.
///
/// Reads no row values. The shape is all the read path needs, and message contents have no
/// business in a diagnostic.
pub fn table_shape(db: &Path, table: &str) -> Result<TableShape> {
    let conn = rusqlite::Connection::open(db)?;
    table_shape_with(&conn, table)
}

fn table_shape_with(conn: &rusqlite::Connection, table: &str) -> Result<TableShape> {
    let mut stmt = conn.prepare(
        "SELECT name, type, \"notnull\", pk FROM pragma_table_info(?1) ORDER BY cid",
    )?;
    let columns = stmt
        .query_map([table], |row| {
            Ok(Column {
                name: row.get(0)?,
                decl_type: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                notnull: row.get::<_, i64>(2)? != 0,
                pk: row.get::<_, i64>(3)? != 0,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // Table names cannot be bound as parameters, so only identifiers SQLite itself reported
    // ever reach this string.
    let rows = conn
        .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |row| {
            row.get::<_, i64>(0)
        })
        .ok();

    Ok(TableShape { name: table.to_string(), columns, rows })
}

/// Shapes for every table, or just the named ones.
pub fn schema_shapes(db: &Path, only: Option<&[String]>) -> Result<Vec<TableShape>> {
    let conn = rusqlite::Connection::open(db)?;
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let tables = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    tables
        .into_iter()
        .filter(|t| only.is_none_or(|wanted| wanted.iter().any(|w| w == t)))
        .map(|t| table_shape_with(&conn, &t))
        .collect()
}

fn parse_version_dir(name: &str) -> Option<u32> {
    let digits = name.strip_prefix('V')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_directories() {
        assert_eq!(parse_version_dir("V10"), Some(10));
        assert_eq!(parse_version_dir("V2"), Some(2));
        assert_eq!(parse_version_dir("V"), None);
        assert_eq!(parse_version_dir("MailData"), None);
        assert_eq!(parse_version_dir("V10x"), None);
    }

    #[test]
    fn discover_picks_highest_version() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["V2", "V9", "V10", "MailData"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        let store = MailStore::discover_in(dir.path()).unwrap();
        assert_eq!(store.version, 10);
        assert!(store.path.ends_with("V10"));
    }

    #[test]
    fn discover_reports_empty_mail_dir() {
        let dir = tempfile::tempdir().unwrap();
        let err = MailStore::discover_in(dir.path()).unwrap_err();
        assert!(matches!(err, Error::NoVersionedStore { .. }));
    }

    #[test]
    fn discover_reports_missing_mail_dir() {
        let dir = tempfile::tempdir().unwrap();
        let err = MailStore::discover_in(&dir.path().join("absent")).unwrap_err();
        assert!(matches!(err, Error::MailDirMissing { .. }));
    }

    #[test]
    fn appends_sidecar_suffix_without_eating_the_extension() {
        let p = Path::new("/tmp/MailData/Envelope Index");
        assert_eq!(with_suffix(p, "-wal").file_name().unwrap(), "Envelope Index-wal");
    }

    #[test]
    fn scan_counts_complete_and_partial_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let store = MailStore { root: dir.path().into(), version: 10, path: dir.path().into() };
        let nested = dir.path().join("Inbox.mbox/Data/1/Messages");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("1.emlx"), b"x").unwrap();
        fs::write(nested.join("2.emlx"), b"x").unwrap();
        fs::write(nested.join("3.partial.emlx"), b"x").unwrap();
        fs::write(nested.join("notes.txt"), b"x").unwrap();

        let scan = store.scan_emlx(100).unwrap();
        assert_eq!(scan.complete_count, 2);
        assert_eq!(scan.partial_count, 1);
        assert!(!scan.capped);
    }

    #[test]
    fn table_shape_reports_columns_and_rows_without_reading_values() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Envelope Index");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, subject INTEGER NOT NULL, flags INTEGER);
             INSERT INTO messages (subject, flags) VALUES (1, 0), (2, 1);",
        )
        .unwrap();
        drop(conn);

        let shape = table_shape(&db, "messages").unwrap();
        assert_eq!(shape.rows, Some(2));
        let names: Vec<_> = shape.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["ROWID", "subject", "flags"]);
        assert!(shape.columns[0].pk);
        assert!(shape.columns[1].notnull);
        assert!(!shape.columns[2].notnull);
        assert_eq!(shape.columns[1].decl_type, "INTEGER");
    }

    #[test]
    fn schema_shapes_can_filter_to_named_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Envelope Index");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (a INTEGER);
             CREATE TABLE subjects (b TEXT);
             CREATE TABLE noise (c TEXT);",
        )
        .unwrap();
        drop(conn);

        let all = schema_shapes(&db, None).unwrap();
        assert_eq!(all.len(), 3);

        let only = vec!["messages".to_string(), "subjects".to_string()];
        let filtered = schema_shapes(&db, Some(&only)).unwrap();
        let names: Vec<_> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["messages", "subjects"]);
    }

    #[test]
    fn inspect_schema_flags_missing_required_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Envelope Index");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (ROWID INTEGER PRIMARY KEY, subject INTEGER, date_sent INTEGER);
             CREATE TABLE subjects (ROWID INTEGER PRIMARY KEY, subject TEXT);
             INSERT INTO messages (subject, date_sent) VALUES (1, 0);",
        )
        .unwrap();
        drop(conn);

        let report = inspect_schema(&db).unwrap();
        assert!(!report.is_usable());
        assert_eq!(report.missing_required, vec!["mailboxes".to_string()]);
        assert_eq!(report.message_count, Some(1));
        assert_eq!(report.message_columns, vec!["ROWID", "subject", "date_sent"]);
    }
}
