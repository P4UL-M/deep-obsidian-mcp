use std::path::{Path, PathBuf};
use std::sync::Once;

use rusqlite::{ffi::sqlite3_auto_extension, Connection, OpenFlags};
use sqlite_vec::sqlite3_vec_init;

pub const INDEX_FILENAME: &str = "index.sqlite";
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

static SQLITE_VEC_REGISTER: Once = Once::new();

pub fn default_index_dir(vault_path: &Path) -> PathBuf {
    vault_path.join(".deep-obsidian-mcp")
}

pub fn default_index_file_path(vault_path: &Path) -> PathBuf {
    default_index_dir(vault_path).join(INDEX_FILENAME)
}

pub fn index_file_path(vault_path: &Path, index_dir: Option<&Path>) -> PathBuf {
    index_dir
        .map(|dir| dir.join(INDEX_FILENAME))
        .unwrap_or_else(|| default_index_file_path(vault_path))
}

fn register_sqlite_vec() {
    SQLITE_VEC_REGISTER.call_once(|| unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())));
    });
}

pub fn open_index_connection(
    index_file: &Path,
    read_only: bool,
) -> std::result::Result<Connection, rusqlite::Error> {
    register_sqlite_vec();

    let mut flags = OpenFlags::SQLITE_OPEN_NO_MUTEX;
    flags |= if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    };
    Connection::open_with_flags(index_file, flags)
}

pub const CURRENT_SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS metadata (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS file_snapshots (
  path TEXT PRIMARY KEY,
  mtime_ms INTEGER NOT NULL,
  size INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS document_frequencies (
  term TEXT PRIMARY KEY,
  df INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS notes (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  title TEXT NOT NULL,
  content TEXT NOT NULL,
  term_counts_json TEXT NOT NULL,
  norm REAL NOT NULL,
  token_count INTEGER NOT NULL,
  links_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL,
  title TEXT NOT NULL,
  chunk_index INTEGER NOT NULL,
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  text TEXT NOT NULL,
  term_counts_json TEXT NOT NULL,
  norm REAL NOT NULL,
  token_count INTEGER NOT NULL,
  UNIQUE (path, chunk_index)
);

CREATE TABLE IF NOT EXISTS chunk_terms (
  term TEXT NOT NULL,
  chunk_id INTEGER NOT NULL,
  PRIMARY KEY (term, chunk_id)
);

CREATE TABLE IF NOT EXISTS embedding_config (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
CREATE INDEX IF NOT EXISTS idx_notes_path ON notes(path);
CREATE INDEX IF NOT EXISTS idx_chunk_terms_chunk_id ON chunk_terms(chunk_id);
"#;

pub fn recreate_vector_tables(
    conn: &Connection,
    embedding_dimensions: Option<usize>,
) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS note_embeddings_vec;
        DROP TABLE IF EXISTS chunk_embeddings_vec;
        "#,
    )?;

    if let Some(dimensions) = embedding_dimensions.filter(|dimensions| *dimensions > 0) {
        conn.execute_batch(&format!(
            r#"
            CREATE VIRTUAL TABLE note_embeddings_vec USING vec0(embedding float[{dimensions}]);
            CREATE VIRTUAL TABLE chunk_embeddings_vec USING vec0(embedding float[{dimensions}]);
            "#
        ))?;
    }

    Ok(())
}

pub fn has_vector_tables(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('note_embeddings_vec', 'chunk_embeddings_vec')",
        [],
        |row| row.get(0),
    )?;
    Ok(count == 2)
}
