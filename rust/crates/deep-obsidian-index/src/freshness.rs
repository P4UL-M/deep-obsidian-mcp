//! Recency before candidate truncation, stored in one D+2 vector table.
//!
//! P=[unit(x), z, sqrt(1-z²)], Q=[(1-beta)unit(q), beta*g, 0].
//! z=2^((mtime-T)/90d), g=2^((T-now)/90d). Never normalize P or Q.
//! Renewal rewrites that table in one ordinary SQLite transaction.
//! No parallel generation or persistent cursor; an interrupted attempt restarts.
use std::collections::BTreeMap;

use chrono::{DateTime, Months, Utc};
use deep_obsidian_types::FreshnessLevel;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::{
    index::{embedding_blob, normalize_dense_vector, IndexError, Result, SearchIndex},
    sqlite::open_index_connection,
};

pub const HALF_LIFE_DAYS: f64 = 90.;
const DAY_MS: f64 = 86_400_000.;
const PAGE_SIZE: i64 = 128;

fn sql(error: rusqlite::Error) -> IndexError {
    IndexError::Embedding(error.to_string())
}

pub fn factor(mtime_ms: u64, now_ms: i64) -> f64 {
    // Unknown timestamps and future clock errors do not confer authority.
    if mtime_ms == 0 || mtime_ms > now_ms.max(0) as u64 {
        return 0.;
    }
    (-(now_ms as f64 - mtime_ms as f64) / (DAY_MS * HALF_LIFE_DAYS)).exp2()
}

pub fn timestamps(index: &SearchIndex) -> BTreeMap<&str, u64> {
    index
        .file_snapshots
        .iter()
        .map(|s| (s.path.as_str(), s.mtime_ms))
        .collect()
}

pub fn keyword_score(raw: f64, level: FreshnessLevel, mtime_ms: u64, now_ms: i64) -> f64 {
    raw * (1. + level.keyword_weight() * factor(mtime_ms, now_ms))
}

/// Common cross-mount scale, independent of the generation's temporal anchor.
pub fn semantic_score(cosine: f64, level: FreshnessLevel, mtime_ms: u64, now_ms: i64) -> f64 {
    let beta = level.semantic_weight();
    let score = (1. - beta) * cosine + beta * factor(mtime_ms, now_ms);
    cosine_score(score)
}

pub fn cosine_score(score: f64) -> f64 {
    1. / (1. + (2. - 2. * score).max(0.).sqrt())
}

/// Only dates and semantic width are persisted, never build/checkpoint state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Encoding {
    pub created_ms: i64,
    pub anchor_ms: i64,
    pub renew_ms: i64,
    pub dimensions: usize,
    #[serde(default)]
    retry_after_ms: Option<i64>,
}
impl Encoding {
    fn new(dimensions: usize, now: DateTime<Utc>) -> Self {
        Self {
            dimensions,
            created_ms: now.timestamp_millis(),
            anchor_ms: (now + Months::new(18)).timestamp_millis(),
            renew_ms: (now + Months::new(16)).timestamp_millis(),
            retry_after_ms: None,
        }
    }
}

pub fn active(conn: &Connection) -> Result<Option<Encoding>> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='metadata')",
            [],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if !exists {
        return Ok(None);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key='freshnessEncoding'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    value
        .map(|v| serde_json::from_str(&v).map_err(|e| IndexError::Embedding(e.to_string())))
        .transpose()
}
fn save(conn: &Connection, encoding: &Encoding) -> Result<()> {
    let value =
        serde_json::to_string(encoding).map_err(|e| IndexError::Embedding(e.to_string()))?;
    conn.execute(
        "INSERT OR REPLACE INTO metadata(key,value) VALUES('freshnessEncoding',?1)",
        [value],
    )
    .map_err(sql)?;
    conn.execute(
        "INSERT OR REPLACE INTO metadata(key,value) VALUES('version',?1)",
        [crate::sqlite::CURRENT_SCHEMA_VERSION.to_string()],
    )
    .map_err(sql)?;
    Ok(())
}
fn note_vector(embedding: &[f64], mtime: u64, e: &Encoding, now: i64) -> Result<Vec<f64>> {
    if embedding.len() != e.dimensions || embedding.iter().any(|x| !x.is_finite()) {
        return Err(IndexError::Embedding("invalid semantic vector".into()));
    }
    let mut p = normalize_dense_vector(embedding);
    let norm2 = p.iter().map(|x| x * x).sum::<f64>();
    let z = if mtime > 0 && mtime <= now.max(0) as u64 && mtime <= e.anchor_ms.max(0) as u64 {
        ((mtime as f64 - e.anchor_ms as f64) / (DAY_MS * HALF_LIFE_DAYS)).exp2()
    } else {
        0.
    };
    p.push(z);
    p.push((2. - norm2 - z * z).max(0.).sqrt());
    Ok(p)
}
fn decode(blob: &[u8], dimensions: usize) -> Result<Vec<f64>> {
    if blob.len() < dimensions * 4 || blob.len() % 4 != 0 {
        return Err(IndexError::Embedding("invalid cached vector length".into()));
    }
    Ok(blob[..dimensions * 4]
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().expect("four bytes")) as f64)
        .collect())
}
fn encode(embedding: &[f64], mtime: u64, e: &mut Encoding, now: i64) -> Result<Vec<u8>> {
    if mtime > now.max(0) as u64 && mtime <= i64::MAX as u64 {
        e.retry_after_ms = Some(
            e.retry_after_ms
                .map_or(mtime as i64, |old| old.min(mtime as i64)),
        );
    }
    Ok(embedding_blob(&note_vector(embedding, mtime, e, now)?))
}
/// Incremental writes put only the augmented vector in the original table.
pub(crate) fn stored_blob(conn: &Connection, embedding: &[f64], mtime: u64) -> Result<Vec<u8>> {
    let Some(mut e) = active(conn)? else {
        return Ok(embedding_blob(embedding));
    };
    let previous = e.retry_after_ms;
    let result = encode(embedding, mtime, &mut e, Utc::now().timestamp_millis())?;
    if previous != e.retry_after_ms {
        save(conn, &e)?;
    }
    Ok(result)
}

pub fn query_vector(
    query: &[f64],
    e: &Encoding,
    level: FreshnessLevel,
    now: i64,
) -> Result<(Vec<f64>, f64)> {
    if query.len() != e.dimensions
        || query.iter().any(|x| !x.is_finite())
        || query.iter().all(|x| *x == 0.)
    {
        return Err(IndexError::Embedding(
            "invalid freshness query vector".into(),
        ));
    }
    let beta = level.semantic_weight();
    let mut q = normalize_dense_vector(query)
        .into_iter()
        .map(|x| (1. - beta) * x)
        .collect::<Vec<_>>();
    q.push(beta * ((e.anchor_ms as f64 - now as f64) / (DAY_MS * HALF_LIFE_DAYS)).exp2());
    q.push(0.);
    if q.iter()
        .any(|x| !x.is_finite() || (*x as f32).is_infinite())
    {
        return Err(IndexError::Embedding(
            "freshness query exceeds float32 range".into(),
        ));
    }
    let norm2 = q.iter().map(|x| x * x).sum();
    Ok((q, norm2))
}
pub fn distance_score(distance: f64, query_norm2: f64) -> f64 {
    cosine_score((2. + query_norm2 - distance * distance) / 2.)
}

/// Remove obsolete prototype tables, and clear the encoding before a full rebuild.
/// The shipped format has only chunk_embeddings_vec, not a second vector copy.
pub(crate) fn clear(conn: &Connection) -> Result<()> {
    let names = {
        let mut s = conn
            .prepare("SELECT name FROM sqlite_master WHERE name GLOB 'freshness_vec_[0-9]*'")
            .map_err(sql)?;
        let rows = s.query_map([], |r| r.get::<_, String>(0)).map_err(sql)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?
    };
    for name in names {
        if name
            .strip_prefix("freshness_vec_")
            .and_then(|s| s.parse::<u64>().ok())
            .is_some()
        {
            conn.execute_batch(&format!("DROP TABLE {name}"))
                .map_err(sql)?;
        }
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS freshness_generations; DROP TABLE IF EXISTS freshness_future_chunks;",
    )
    .map_err(sql)?;
    if active(conn)?.is_some() {
        conn.execute("DELETE FROM metadata WHERE key='freshnessEncoding'", [])
            .map_err(sql)?;
    }
    Ok(())
}
pub fn configure(index: &mut SearchIndex, level: FreshnessLevel) -> Result<()> {
    // Disabled uses the SAME D+2 representation with beta=0.
    if index.embedding_dimensions.is_some() {
        maintain_now(index)?;
        index.version = crate::sqlite::CURRENT_SCHEMA_VERSION;
    }
    index.freshness = level;
    Ok(())
}
pub fn maintain_now(index: &SearchIndex) -> Result<bool> {
    maintain(index, Utc::now())
}

/// One table, one normal transaction. No resume protocol or serving guarantee.
/// Legacy D vectors are converted once from cache, without a model request.
pub fn maintain(index: &SearchIndex, now: DateTime<Utc>) -> Result<bool> {
    let Some(dimensions) = index.embedding_dimensions else {
        return Ok(false);
    };
    let context = index
        .context
        .as_ref()
        .ok_or(IndexError::MissingIndexContext)?;
    let mut conn = open_index_connection(&context.index_file, false).map_err(sql)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let previous = active(&tx)?;
    if previous.as_ref().is_some_and(|e| {
        e.dimensions == dimensions
            && e.renew_ms > now.timestamp_millis()
            && e.retry_after_ms.is_none_or(|t| t > now.timestamp_millis())
    }) {
        return Ok(false);
    }
    let mut e = match &previous {
        Some(e) if e.dimensions == dimensions && e.renew_ms > now.timestamp_millis() => e.clone(),
        _ => Encoding::new(dimensions, now),
    };
    e.retry_after_ms = None;
    if previous.is_none() {
        // One-time D -> D+2 conversion needs the cached source rows in RAM while
        // replacing the virtual table. No parallel persistent table is created.
        let rows = {
            let mut s=tx.prepare("SELECT v.rowid,v.embedding,COALESCE(s.mtime_ms,0) FROM chunk_embeddings_vec v JOIN chunks c ON c.id=v.rowid LEFT JOIN file_snapshots s ON s.path=c.path").map_err(sql)?;
            let rows = s
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(sql)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?
        };
        clear(&tx)?;
        tx.execute_batch(&format!("DROP TABLE chunk_embeddings_vec; CREATE VIRTUAL TABLE chunk_embeddings_vec USING vec0(embedding float[{}]);",dimensions+2)).map_err(sql)?;
        for (id, blob, mtime) in rows {
            let blob = encode(
                &decode(&blob, dimensions)?,
                mtime.max(0) as u64,
                &mut e,
                now.timestamp_millis(),
            )?;
            tx.execute(
                "INSERT INTO chunk_embeddings_vec(rowid,embedding) VALUES(?1,?2)",
                params![id, blob],
            )
            .map_err(sql)?;
        }
    } else {
        // Bounded memory, but no committed intermediate cursor: on failure SQLite
        // rolls back the attempt and the next check starts at the beginning.
        let mut cursor = 0;
        loop {
            let rows = {
                let mut s=tx.prepare("SELECT c.id,v.embedding,COALESCE(s.mtime_ms,0) FROM chunks c CROSS JOIN chunk_embeddings_vec v ON v.rowid=c.id LEFT JOIN file_snapshots s ON s.path=c.path WHERE c.id>?1 ORDER BY c.id LIMIT ?2").map_err(sql)?;
                let rows = s
                    .query_map(params![cursor, PAGE_SIZE], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Vec<u8>>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })
                    .map_err(sql)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?
            };
            if rows.is_empty() {
                break;
            }
            for (id, blob, mtime) in rows {
                let blob = encode(
                    &decode(&blob, dimensions)?,
                    mtime.max(0) as u64,
                    &mut e,
                    now.timestamp_millis(),
                )?;
                tx.execute("DELETE FROM chunk_embeddings_vec WHERE rowid=?1", [id])
                    .map_err(sql)?;
                tx.execute(
                    "INSERT INTO chunk_embeddings_vec(rowid,embedding) VALUES(?1,?2)",
                    params![id, blob],
                )
                .map_err(sql)?;
                cursor = id;
            }
        }
    }
    save(&tx, &e)?;
    tx.commit().map_err(sql)?;
    Ok(true)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        index::{build_index, SemanticBackend},
        search::{
            bm25_search_with_options, semantic_scores_for_chunks,
            semantic_search_with_query_vector_sql, RankingOptions,
        },
    };
    use std::{fs, path::PathBuf};

    struct Fixture {
        dir: PathBuf,
        index: SearchIndex,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
    impl Fixture {
        fn new(count: usize) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "freshness-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            for i in 0..count {
                fs::write(
                    dir.join(format!("note-{i:04}.md")),
                    "# Shared decision\n\nStorage strategy decision.\n",
                )
                .unwrap();
            }
            let mut index = build_index(&dir, None, None).unwrap();
            assert_eq!(index.chunk_count, count);
            index.embedding_dimensions = Some(2);
            index.semantic_backend = SemanticBackend::Embedding;
            let mut conn =
                open_index_connection(&index.context.as_ref().unwrap().index_file, false).unwrap();
            conn.execute_batch(
                "CREATE VIRTUAL TABLE chunk_embeddings_vec USING vec0(embedding float[2])",
            )
            .unwrap();
            let now = Utc::now();
            let tx = conn.transaction().unwrap();
            for (i, snapshot) in index.file_snapshots.iter_mut().enumerate() {
                snapshot.mtime_ms = (now
                    - chrono::Duration::days(if i + 1 == count { 14 } else { 730 }))
                .timestamp_millis() as u64;
                tx.execute(
                    "UPDATE file_snapshots SET mtime_ms=?1 WHERE path=?2",
                    params![snapshot.mtime_ms as i64, snapshot.path],
                )
                .unwrap();
            }
            for i in 0..count {
                let embedding = if i + 1 == count {
                    vec![0.99995, (1.0_f64 - 0.99995_f64.powi(2)).sqrt()]
                } else {
                    vec![1., 0.]
                };
                tx.execute(
                    "INSERT INTO chunk_embeddings_vec(rowid,embedding) VALUES(?1,?2)",
                    params![i as i64 + 1, embedding_blob(&embedding)],
                )
                .unwrap();
            }
            tx.commit().unwrap();
            Self { dir, index }
        }
        fn conn(&self) -> Connection {
            open_index_connection(&self.index.context.as_ref().unwrap().index_file, false).unwrap()
        }
    }

    #[test]
    fn one_vector_table_supports_recency_and_beta_zero() {
        let mut f = Fixture::new(300);
        let options = RankingOptions {
            limit: 64,
            ..Default::default()
        };
        let before =
            bm25_search_with_options(&f.index, "storage strategy", options.clone()).unwrap();
        let original = semantic_search_with_query_vector_sql(&f.index, &[1., 0.], 64).unwrap();
        let related_before = crate::search::related_notes(&f.index, "note-0299.md").unwrap();
        assert!(!original.iter().any(|h| h.path == "note-0299.md"));
        configure(&mut f.index, FreshnessLevel::Classic).unwrap();
        let conn = f.conn();
        let encoding = active(&conn).unwrap().unwrap();
        let length: i64 = conn
            .query_row(
                "SELECT vec_length(embedding) FROM chunk_embeddings_vec LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(length, 4);
        let related_after = crate::search::related_notes(&f.index, "note-0299.md").unwrap();
        assert!(!related_after.is_empty());
        assert!(
            (related_after[0].score - related_before[0].score).abs() < 2e-5,
            "related notes ignore both temporal coordinates"
        );
        let other_tables:i64=conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name GLOB 'freshness_vec_*' OR name IN ('freshness_generations','freshness_future_chunks')",[],|r|r.get(0)).unwrap();
        assert_eq!(
            other_tables, 0,
            "no duplicate vector table, build state or persistent cursor"
        );
        let hits = semantic_search_with_query_vector_sql(&f.index, &[1., 0.], 64).unwrap();
        assert_eq!(hits[0].path, "note-0299.md");
        assert_eq!(
            bm25_search_with_options(&f.index, "storage strategy", options.clone()).unwrap()[0]
                .path,
            "note-0299.md"
        );
        let keys = hits
            .iter()
            .map(|h| (h.path.clone(), h.chunk_index))
            .collect::<Vec<_>>();
        for (hit, exact) in hits
            .iter()
            .zip(semantic_scores_for_chunks(&f.index, &[1., 0.], &keys).unwrap())
        {
            assert!((hit.score - exact.unwrap()).abs() < 3e-6);
        }
        configure(&mut f.index, FreshnessLevel::Disabled).unwrap();
        assert_eq!(
            active(&conn).unwrap().unwrap().created_ms,
            encoding.created_ms,
            "beta zero doesn't rebuild"
        );
        let disabled = semantic_search_with_query_vector_sql(&f.index, &[1., 0.], 64).unwrap();
        assert!(!disabled.iter().any(|h| h.path == "note-0299.md"));
        assert!((disabled[0].score - original[0].score).abs() < 3e-6);
        assert_eq!(
            before,
            bm25_search_with_options(&f.index, "storage strategy", options).unwrap()
        );
        let length: i64 = conn
            .query_row(
                "SELECT vec_length(embedding) FROM chunk_embeddings_vec LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            length, 4,
            "disabled still uses the same augmented representation"
        );
    }

    #[test]
    fn renewal_is_in_place_and_a_failed_attempt_restarts_without_checkpoint() {
        let mut f = Fixture::new(300);
        configure(&mut f.index, FreshnessLevel::Classic).unwrap();
        let conn = f.conn();
        let old = active(&conn).unwrap().unwrap();
        let original_blob: Vec<u8> = conn
            .query_row(
                "SELECT embedding FROM chunk_embeddings_vec WHERE rowid=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let due = DateTime::from_timestamp_millis(old.renew_ms).unwrap();
        conn.execute_batch("CREATE TRIGGER interrupt_renewal BEFORE INSERT ON metadata WHEN NEW.key='freshnessEncoding' BEGIN SELECT RAISE(ABORT,'interruption'); END").unwrap();
        assert!(maintain(&f.index, due).is_err());
        assert_eq!(active(&conn).unwrap().unwrap().anchor_ms, old.anchor_ms);
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT embedding FROM chunk_embeddings_vec WHERE rowid=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            blob, original_blob,
            "ordinary SQLite rollback, not a partial generation"
        );
        conn.execute_batch("DROP TRIGGER interrupt_renewal")
            .unwrap();
        maintain(&f.index, due).unwrap();
        let next = active(&conn).unwrap().unwrap();
        assert_eq!(next.created_ms, due.timestamp_millis());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunk_embeddings_vec", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 300);
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT embedding FROM chunk_embeddings_vec WHERE rowid=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(&blob[..8], &original_blob[..8]);
        assert_ne!(&blob[8..], &original_blob[8..]);
        maintain(&f.index, due + chrono::Duration::days(1)).unwrap();
        assert_eq!(active(&conn).unwrap().unwrap().anchor_ms, next.anchor_ms);
    }
    #[test]
    fn calendar_deadlines_and_the_euclidean_identity_hold() {
        let now = "2026-01-31T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let e = Encoding::new(2, now);
        assert_eq!(
            DateTime::from_timestamp_millis(e.anchor_ms)
                .unwrap()
                .date_naive()
                .to_string(),
            "2027-07-31"
        );
        assert_eq!(
            DateTime::from_timestamp_millis(e.renew_ms)
                .unwrap()
                .date_naive()
                .to_string(),
            "2027-05-31"
        );
        let mtime = (now - chrono::Duration::days(90)).timestamp_millis() as u64;
        assert!((factor(mtime, now.timestamp_millis()) - 0.5).abs() < 1e-12);
        let p = note_vector(&[1., 0.], mtime, &e, now.timestamp_millis()).unwrap();
        for level in [FreshnessLevel::Disabled, FreshnessLevel::Classic] {
            let (q, norm) = query_vector(&[1., 0.], &e, level, now.timestamp_millis()).unwrap();
            let distance = p
                .iter()
                .zip(q)
                .map(|(a, b)| (a - b) * (a - b))
                .sum::<f64>()
                .sqrt();
            assert!(
                (distance_score(distance, norm)
                    - semantic_score(1., level, mtime, now.timestamp_millis()))
                .abs()
                    < 1e-12
            );
        }
    }
}
