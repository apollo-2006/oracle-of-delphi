//! Episodic + vector store over SQLite (architecture §5.2).

use super::embed::Embedder;
use super::{cosine, reciprocal_rank_fusion};
use rusqlite::{params, Connection};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeKind {
    Conversation,
    Action,
    Observation,
}

impl EpisodeKind {
    fn as_str(&self) -> &'static str {
        match self {
            EpisodeKind::Conversation => "conversation",
            EpisodeKind::Action => "action",
            EpisodeKind::Observation => "observation",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Episode {
    pub id: i64,
    pub kind: EpisodeKind,
    pub text: String,
    pub t_unix: i64,
    pub salience: f32,
}

#[derive(Debug, Clone)]
pub struct RetrievedItem {
    pub episode: Episode,
    pub score: f32,
}

/// Owns the SQLite connection. Single-writer by construction (core is the only
/// writer), guarded by a mutex; reads go through the same lock for simplicity.
pub struct MemoryStore {
    conn: Mutex<Connection>,
    embedder: Box<dyn Embedder>,
}

impl MemoryStore {
    /// Open (or create) a store at `path` (":memory:" for tests).
    pub fn open(path: &str, embedder: Box<dyn Embedder>) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate_embed_model(&conn)?;
        Ok(MemoryStore {
            conn: Mutex::new(conn),
            embedder,
        })
    }

    /// Insert an episode, embedding its text. Returns the new row id.
    /// Insert an episode, embedding its text. Returns the new row id.
    ///
    /// Propagates an embedding failure instead of storing the row unembedded:
    /// a row with no usable vector is invisible to retrieval but still counts
    /// against every scan, which is the worst of both.
    pub fn insert(&self, kind: EpisodeKind, text: &str, salience: f32) -> anyhow::Result<i64> {
        let emb = self.embedder.embed(text)?;
        let blob = f32_to_bytes(&emb);
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO episode(kind, text, t_unix, salience, embedding, embed_model) \
             VALUES(?,?,?,?,?,?)",
            params![kind.as_str(), text, now, salience, blob, self.embedder.id()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// How many episodes are in a vector space other than the active one.
    ///
    /// Non-zero means the embedder changed and that many memories are currently
    /// reachable by keyword but not by meaning. Surfaced so it can be reported
    /// and re-embedded rather than discovered as "recall got worse".
    pub fn stale_space_count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM episode WHERE embed_model <> ?",
            params![self.embedder.id()],
            |r| r.get(0),
        )?)
    }

    /// Hybrid retrieval: vector top-N ∪ keyword top-N, fused by RRF, re-scored by
    /// cosine, salience-weighted, top-`limit` returned. Dates are the caller's
    /// job to stamp into the prompt.
    pub fn retrieve(&self, query: &str, limit: usize) -> anyhow::Result<Vec<RetrievedItem>> {
        let q_emb = self.embedder.embed(query)?;
        let active_space = self.embedder.id();
        let conn = self.conn.lock().unwrap();

        // Pull candidate rows (in a real deployment this is an HNSW/FTS prefilter;
        // for the reference impl we scan, which is fine at personal scale and
        // keeps the dependency surface tiny).
        let mut stmt = conn.prepare(
            "SELECT id, kind, text, t_unix, salience, embedding, embed_model FROM episode",
        )?;
        let rows = stmt.query_map([], |r| {
            let blob: Vec<u8> = r.get(5)?;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, f64>(4)? as f32,
                bytes_to_f32(&blob),
                r.get::<_, String>(6)?,
            ))
        })?;

        let mut cands = Vec::new();
        for row in rows {
            let (id, kind, text, t_unix, salience, emb, space) = row?;
            // Cosine is only meaningful inside one vector space. A row written
            // by a different embedder scores 0 here rather than a plausible-
            // looking number -- it stays reachable by keyword below, so history
            // does not vanish when the embedder changes, but it never
            // contributes noise dressed up as similarity.
            let vscore = if space == active_space {
                cosine(&q_emb, &emb)
            } else {
                0.0
            };
            let kscore = keyword_overlap(query, &text);
            cands.push((id, kind, text, t_unix, salience, vscore, kscore));
        }

        // Rank lists for RRF.
        // Foreign-space rows are excluded from the vector rank list entirely.
        // Leaving them in at 0.0 would still give them an RRF rank, letting an
        // unrelated memory place purely because the list had room.
        let mut by_vec: Vec<_> = cands
            .iter()
            .filter(|c| c.5 > 0.0)
            .map(|c| (c.0, c.5))
            .collect();
        by_vec.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let vec_ids: Vec<i64> = by_vec.iter().map(|(id, _)| *id).collect();

        let mut by_kw: Vec<_> = cands
            .iter()
            .filter(|c| c.6 > 0.0)
            .map(|c| (c.0, c.6))
            .collect();
        by_kw.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let kw_ids: Vec<i64> = by_kw.iter().map(|(id, _)| *id).collect();

        let fused = reciprocal_rank_fusion(&[vec_ids, kw_ids], 60.0);

        let mut out = Vec::new();
        for id in fused.into_iter().take(limit) {
            if let Some(c) = cands.iter().find(|c| c.0 == id) {
                let kind = match c.1.as_str() {
                    "action" => EpisodeKind::Action,
                    "observation" => EpisodeKind::Observation,
                    _ => EpisodeKind::Conversation,
                };
                // Final score blends similarity with a mild salience boost.
                let score = c.5 * 0.85 + c.4 * 0.15;
                out.push(RetrievedItem {
                    episode: Episode {
                        id: c.0,
                        kind,
                        text: c.2.clone(),
                        t_unix: c.3,
                        salience: c.4,
                    },
                    score,
                });
            }
        }
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        Ok(out)
    }

    /// Reinforce an episode's salience when it gets retrieved-and-used (§5.2
    /// forgetting curve). Capped at 1.0.
    pub fn reinforce(&self, id: i64, delta: f32) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE episode SET salience = MIN(1.0, salience + ?) WHERE id = ?",
            params![delta, id],
        )?;
        Ok(())
    }

    /// Tombstone an episode ("forget that"). Keeps the row for audit but blanks
    /// the text + zeroes the embedding so it can never be retrieved again.
    pub fn tombstone(&self, id: i64) -> anyhow::Result<()> {
        let zero = f32_to_bytes(&vec![0.0; self.embedder.dim()]);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE episode SET text='[forgotten]', salience=0.0, embedding=?, embed_model=? \
             WHERE id=?",
            params![zero, self.embedder.id(), id],
        )?;
        Ok(())
    }

    /// Episodes that have not yet been through the consolidation pass, oldest
    /// first.
    ///
    /// Oldest first because observations expire on a timer: the ones closest to
    /// deletion are the ones whose durable facts are about to be lost, so they
    /// are the ones worth reading now.
    pub fn unconsolidated(&self, limit: usize) -> anyhow::Result<Vec<Episode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, text, t_unix, salience FROM episode \
             WHERE consolidated_at IS NULL AND text <> '[forgotten]' \
             ORDER BY t_unix ASC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(Episode {
                id: r.get(0)?,
                kind: match r.get::<_, String>(1)?.as_str() {
                    "action" => EpisodeKind::Action,
                    "observation" => EpisodeKind::Observation,
                    _ => EpisodeKind::Conversation,
                },
                text: r.get(2)?,
                t_unix: r.get(3)?,
                salience: r.get::<_, f64>(4)? as f32,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Mark episodes as having been through consolidation.
    ///
    /// Called even when nothing was extracted from them. An episode that yields
    /// no facts is not a failure to retry — it is a fact about the episode, and
    /// leaving it unmarked would make every pass re-read the same barren rows
    /// forever while genuinely new ones queue behind them.
    pub fn mark_consolidated(&self, ids: &[i64], now: i64) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE episode SET consolidated_at = ? WHERE id = ?")?;
            for id in ids {
                stmt.execute(params![now, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// How many episodes are still waiting to be consolidated.
    pub fn unconsolidated_count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM episode WHERE consolidated_at IS NULL",
            [],
            |r| r.get(0),
        )?)
    }

    /// The most recent screen observations, newest first.
    ///
    /// Deliberately a **recency** query, not a similarity one. "What am I
    /// looking at right now" is answered by the latest observation regardless of
    /// how its wording scores against the question — embedding similarity ranks
    /// by topic, and the topic of the current screen is exactly what the user
    /// does not know yet. Similarity recall handles "what was I reading on
    /// Tuesday"; this handles "right now".
    pub fn recent_observations(&self, limit: usize, since: i64) -> anyhow::Result<Vec<Episode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, text, t_unix, salience FROM episode \
             WHERE kind = 'observation' AND t_unix >= ? AND text <> '[forgotten]' \
             ORDER BY t_unix DESC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![since, limit as i64], |r| {
            Ok(Episode {
                id: r.get(0)?,
                kind: EpisodeKind::Observation,
                text: r.get(2)?,
                t_unix: r.get(3)?,
                salience: r.get::<_, f64>(4)? as f32,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Delete observation episodes older than `cutoff` (unix seconds).
    ///
    /// Scoped to `observation` on purpose. Conversation memories are things the
    /// user actually said and are never swept on a timer; only the ambient
    /// index produces rows fast enough to need expiry.
    pub fn purge_observations_before(&self, cutoff: i64) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM episode WHERE kind = 'observation' AND t_unix < ?",
            params![cutoff],
        )?)
    }

    pub fn count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))?)
    }
}

/// Add `embed_model` to a database created before vector spaces were tracked.
///
/// `CREATE TABLE IF NOT EXISTS` does not alter an existing table, so a store
/// written by an older build has every other column and none of this one. Those
/// rows were all written by the hash embedder, which is exactly what the column
/// default says, so the migration is a pure add.
fn migrate_embed_model(conn: &Connection) -> anyhow::Result<()> {
    let present: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('episode') WHERE name = 'embed_model'")?
        .exists([])?;
    if !present {
        conn.execute_batch(
            "ALTER TABLE episode ADD COLUMN embed_model TEXT NOT NULL DEFAULT 'hash-384';",
        )?;
        tracing::info!("[memory] migrated: episodes tagged with their vector space");
    }
    // Created here in both cases -- after the column is guaranteed to exist.
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_episode_space ON episode(embed_model);")?;

    // Same story for consolidation state. Existing rows get NULL, i.e. pending,
    // which is the right answer: nothing has been consolidated before now.
    let has_consolidated: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('episode') WHERE name = 'consolidated_at'")?
        .exists([])?;
    if !has_consolidated {
        conn.execute_batch("ALTER TABLE episode ADD COLUMN consolidated_at INTEGER;")?;
        tracing::info!("[memory] migrated: episodes track consolidation state");
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_episode_pending \
         ON episode(consolidated_at) WHERE consolidated_at IS NULL;",
    )?;
    Ok(())
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS episode (
    id          INTEGER PRIMARY KEY,
    kind        TEXT NOT NULL,
    text        TEXT NOT NULL,
    t_unix      INTEGER NOT NULL,
    salience    REAL NOT NULL DEFAULT 0.5,
    embedding   BLOB NOT NULL,
    -- Which embedder produced `embedding`. Cosine is only ever taken between
    -- rows sharing this value; see memory::embed for why 384 == 384 is not
    -- enough to make two vectors comparable.
    embed_model TEXT NOT NULL DEFAULT 'hash-384',
    -- When this episode was folded into the knowledge graph. NULL = pending.
    -- Nullable rather than a boolean so the pass is auditable: "consolidated
    -- when?" is the question you ask when a fact looks wrong.
    consolidated_at INTEGER
);
-- NB: the index on embed_model is created by `migrate_embed_model`, not here.
-- On a database that predates the column, this batch runs against the OLD
-- table (CREATE TABLE IF NOT EXISTS is a no-op), so indexing embed_model at
-- this point fails before the migration has had a chance to add it.
CREATE INDEX IF NOT EXISTS idx_episode_time ON episode(t_unix);

CREATE TABLE IF NOT EXISTS kg_node (
    id    INTEGER PRIMARY KEY,
    type  TEXT NOT NULL,
    name  TEXT NOT NULL,
    props TEXT NOT NULL DEFAULT '{}'
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_node_name ON kg_node(type, name);

CREATE TABLE IF NOT EXISTS kg_alias (
    node_id INTEGER NOT NULL REFERENCES kg_node(id),
    alias   TEXT NOT NULL,
    UNIQUE(alias)
);

CREATE TABLE IF NOT EXISTS kg_edge (
    id           INTEGER PRIMARY KEY,
    src          INTEGER NOT NULL REFERENCES kg_node(id),
    rel          TEXT NOT NULL,
    dst          INTEGER NOT NULL REFERENCES kg_node(id),
    t_valid_from INTEGER NOT NULL,
    t_valid_to   INTEGER,
    provenance   TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_edge_src ON kg_edge(src, rel);
CREATE INDEX IF NOT EXISTS idx_edge_dst ON kg_edge(dst, rel);
"#;

/// The KG-only portion of the schema, exposed so `KnowledgeGraph` can ensure it
/// exists when it opens the DB independently of `MemoryStore`.
pub(crate) fn graph_schema() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS kg_node (
    id    INTEGER PRIMARY KEY,
    type  TEXT NOT NULL,
    name  TEXT NOT NULL,
    props TEXT NOT NULL DEFAULT '{}'
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_node_name ON kg_node(type, name);
CREATE TABLE IF NOT EXISTS kg_alias (
    node_id INTEGER NOT NULL REFERENCES kg_node(id),
    alias   TEXT NOT NULL,
    UNIQUE(alias)
);
CREATE TABLE IF NOT EXISTS kg_edge (
    id           INTEGER PRIMARY KEY,
    src          INTEGER NOT NULL REFERENCES kg_node(id),
    rel          TEXT NOT NULL,
    dst          INTEGER NOT NULL REFERENCES kg_node(id),
    t_valid_from INTEGER NOT NULL,
    t_valid_to   INTEGER,
    provenance   TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_edge_src ON kg_edge(src, rel);
CREATE INDEX IF NOT EXISTS idx_edge_dst ON kg_edge(dst, rel);
"#
}

fn f32_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn bytes_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cheap keyword overlap: fraction of query tokens present in the text.
fn keyword_overlap(query: &str, text: &str) -> f32 {
    let toks: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() > 2)
        .map(|s| s.to_lowercase())
        .collect();
    if toks.is_empty() {
        return 0.0;
    }
    let lower = text.to_lowercase();
    let hits = toks.iter().filter(|t| lower.contains(*t)).count();
    hits as f32 / toks.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::embed::HashEmbedder;

    fn store() -> MemoryStore {
        MemoryStore::open(":memory:", Box::new(HashEmbedder::default())).unwrap()
    }

    /// A hash embedder wearing a different vector-space name. Stands in for
    /// "the user switched to BGE" without needing a sidecar: what matters to
    /// the store is only whether two rows claim the same space.
    struct Renamed(&'static str);

    impl Embedder for Renamed {
        fn dim(&self) -> usize {
            crate::memory::embed::EMBED_DIM
        }
        fn id(&self) -> &str {
            self.0
        }
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            HashEmbedder::default().embed(text)
        }
    }

    #[test]
    fn rows_record_the_space_that_produced_them() {
        let s = store();
        s.insert(EpisodeKind::Conversation, "the bedroom lights", 0.5)
            .unwrap();
        assert_eq!(s.stale_space_count().unwrap(), 0);
    }

    #[test]
    fn switching_embedders_makes_old_rows_stale_not_wrong() {
        // Same file, two embedders. The vectors are byte-identical here, which
        // is the point: the store must refuse to compare them on the strength
        // of the recorded space alone, not on whether the numbers happen to
        // line up.
        let path = tempfile::NamedTempFile::new().unwrap();
        let db = path.path().to_str().unwrap();

        let old = MemoryStore::open(db, Box::new(Renamed("hash-384"))).unwrap();
        old.insert(
            EpisodeKind::Conversation,
            "user asked to dim the bedroom lights",
            0.5,
        )
        .unwrap();
        drop(old);

        let new = MemoryStore::open(db, Box::new(Renamed("bge-small-384"))).unwrap();
        assert_eq!(
            new.stale_space_count().unwrap(),
            1,
            "the old row is in a foreign space and must be reported as such"
        );

        // Keyword retrieval is space-independent, so history is still reachable
        // rather than silently gone.
        let hits = new.retrieve("dim the bedroom lights", 5).unwrap();
        assert_eq!(hits.len(), 1, "keyword must still find the old row");

        // ...but it must not have scored on cosine. With the same underlying
        // vectors, an in-space row would score near 1.0; the blend caps a
        // keyword-only hit far below that.
        assert!(
            hits[0].score < 0.5,
            "a foreign-space row must not score on similarity, got {}",
            hits[0].score
        );
    }

    #[test]
    fn a_pre_migration_database_gains_the_column_and_keeps_its_rows() {
        // A store written by a build that predates vector-space tracking. The
        // failure this guards is real: CREATE TABLE IF NOT EXISTS silently does
        // nothing to an existing table, so without an explicit migration every
        // query naming embed_model would fail at startup on a live database.
        let path = tempfile::NamedTempFile::new().unwrap();
        let db = path.path().to_str().unwrap();
        {
            let conn = rusqlite::Connection::open(db).unwrap();
            conn.execute_batch(
                "CREATE TABLE episode (
                     id        INTEGER PRIMARY KEY,
                     kind      TEXT NOT NULL,
                     text      TEXT NOT NULL,
                     t_unix    INTEGER NOT NULL,
                     salience  REAL NOT NULL DEFAULT 0.5,
                     embedding BLOB NOT NULL
                 );
                 INSERT INTO episode(kind, text, t_unix, salience, embedding)
                 VALUES('conversation', 'an older memory', 1, 0.5, x'00');",
            )
            .unwrap();
        }

        let s = MemoryStore::open(db, Box::new(HashEmbedder::default())).unwrap();
        assert_eq!(s.count().unwrap(), 1, "the old row survives the migration");
        // Pre-existing rows were all hash-embedded, which is what the column
        // default claims, so nothing is stale immediately after migrating.
        assert_eq!(s.stale_space_count().unwrap(), 0);
    }

    #[test]
    fn retention_expires_only_old_observations() {
        let s = store();
        let a = s
            .insert(EpisodeKind::Observation, "an old screenshot", 0.2)
            .unwrap();
        let b = s
            .insert(EpisodeKind::Conversation, "something the user said", 0.8)
            .unwrap();
        {
            let conn = s.conn.lock().unwrap();
            // Backdate both well past any cutoff.
            conn.execute("UPDATE episode SET t_unix = 100", []).unwrap();
        }
        let removed = s.purge_observations_before(1_000).unwrap();
        assert_eq!(removed, 1, "only the observation is swept");
        assert_eq!(s.count().unwrap(), 1);
        let _ = (a, b);
    }

    #[test]
    fn retention_keeps_observations_inside_the_window() {
        let s = store();
        s.insert(EpisodeKind::Observation, "a recent screenshot", 0.2)
            .unwrap();
        // Cutoff in the distant past: nothing is old enough yet.
        assert_eq!(s.purge_observations_before(0).unwrap(), 0);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn new_episodes_are_pending_consolidation() {
        let s = store();
        s.insert(EpisodeKind::Observation, "a screenshot", 0.2)
            .unwrap();
        assert_eq!(s.unconsolidated_count().unwrap(), 1);
        assert_eq!(s.unconsolidated(10).unwrap().len(), 1);
    }

    #[test]
    fn marking_removes_an_episode_from_the_pending_queue() {
        let s = store();
        let id = s
            .insert(EpisodeKind::Conversation, "my advisor is Dr Chen", 0.8)
            .unwrap();
        s.mark_consolidated(&[id], 1_000).unwrap();
        assert_eq!(s.unconsolidated_count().unwrap(), 0);
        assert!(s.unconsolidated(10).unwrap().is_empty());
    }

    #[test]
    fn pending_episodes_come_back_oldest_first() {
        // Observations expire on a timer, so the ones nearest deletion are the
        // ones whose durable facts are about to be lost for good.
        let s = store();
        let a = s.insert(EpisodeKind::Observation, "first", 0.2).unwrap();
        let b = s.insert(EpisodeKind::Observation, "second", 0.2).unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute("UPDATE episode SET t_unix = 100 WHERE id = ?", params![a])
                .unwrap();
            conn.execute("UPDATE episode SET t_unix = 200 WHERE id = ?", params![b])
                .unwrap();
        }
        let pending = s.unconsolidated(10).unwrap();
        assert_eq!(pending[0].id, a);
        assert_eq!(pending[1].id, b);
    }

    #[test]
    fn a_forgotten_episode_is_never_consolidated() {
        // "Forget that" must mean it, including not mining the tombstone for
        // facts on the next background pass.
        let s = store();
        let id = s
            .insert(EpisodeKind::Conversation, "something private", 0.9)
            .unwrap();
        s.tombstone(id).unwrap();
        assert!(s.unconsolidated(10).unwrap().is_empty());
    }

    #[test]
    fn marking_an_empty_set_is_a_no_op() {
        let s = store();
        s.insert(EpisodeKind::Observation, "x", 0.2).unwrap();
        s.mark_consolidated(&[], 1_000).unwrap();
        assert_eq!(s.unconsolidated_count().unwrap(), 1);
    }

    #[test]
    fn a_pre_migration_database_starts_fully_pending() {
        // Every row in an existing store predates consolidation, so all of them
        // are legitimately pending -- NULL is the correct default, not a bug.
        let path = tempfile::NamedTempFile::new().unwrap();
        let db = path.path().to_str().unwrap();
        {
            let conn = rusqlite::Connection::open(db).unwrap();
            conn.execute_batch(
                "CREATE TABLE episode (
                     id        INTEGER PRIMARY KEY,
                     kind      TEXT NOT NULL,
                     text      TEXT NOT NULL,
                     t_unix    INTEGER NOT NULL,
                     salience  REAL NOT NULL DEFAULT 0.5,
                     embedding BLOB NOT NULL
                 );
                 INSERT INTO episode(kind, text, t_unix, salience, embedding)
                 VALUES('conversation', 'an older memory', 1, 0.5, x'00');",
            )
            .unwrap();
        }
        let s = MemoryStore::open(db, Box::new(HashEmbedder::default())).unwrap();
        assert_eq!(s.unconsolidated_count().unwrap(), 1);
    }

    #[test]
    fn recent_observations_come_back_newest_first() {
        let s = store();
        let a = s
            .insert(EpisodeKind::Observation, "older screen", 0.2)
            .unwrap();
        let b = s
            .insert(EpisodeKind::Observation, "newer screen", 0.2)
            .unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute("UPDATE episode SET t_unix = 100 WHERE id = ?", params![a])
                .unwrap();
            conn.execute("UPDATE episode SET t_unix = 200 WHERE id = ?", params![b])
                .unwrap();
        }
        let recent = s.recent_observations(5, 0).unwrap();
        assert_eq!(recent[0].id, b, "newest first");
        assert_eq!(recent[1].id, a);
    }

    #[test]
    fn recent_observations_respect_the_age_cutoff() {
        // A screen from three hours ago is not what "right now" means, and
        // offering it as current context is how the assistant confidently
        // describes a window you closed at lunch.
        let s = store();
        let a = s
            .insert(EpisodeKind::Observation, "stale screen", 0.2)
            .unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute("UPDATE episode SET t_unix = 100 WHERE id = ?", params![a])
                .unwrap();
        }
        assert!(s.recent_observations(5, 1_000).unwrap().is_empty());
    }

    #[test]
    fn recent_observations_exclude_conversations_and_tombstones() {
        let s = store();
        s.insert(EpisodeKind::Conversation, "the user said something", 0.8)
            .unwrap();
        let obs = s.insert(EpisodeKind::Observation, "a screen", 0.2).unwrap();
        s.tombstone(obs).unwrap();
        assert!(s.recent_observations(5, 0).unwrap().is_empty());
    }

    #[test]
    fn an_embedding_failure_writes_no_row() {
        // Better to lose one episode than to store one that retrieval can never
        // see but every scan still pays for.
        struct Broken;
        impl Embedder for Broken {
            fn dim(&self) -> usize {
                crate::memory::embed::EMBED_DIM
            }
            fn id(&self) -> &str {
                "broken"
            }
            fn embed(&self, _: &str) -> anyhow::Result<Vec<f32>> {
                anyhow::bail!("sidecar is down")
            }
        }
        let s = MemoryStore::open(":memory:", Box::new(Broken)).unwrap();
        assert!(s.insert(EpisodeKind::Conversation, "hello", 0.5).is_err());
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn insert_and_count() {
        let s = store();
        s.insert(EpisodeKind::Conversation, "hello world", 0.5)
            .unwrap();
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn retrieval_finds_relevant_episode() {
        let s = store();
        s.insert(
            EpisodeKind::Conversation,
            "user asked to dim the bedroom lights",
            0.5,
        )
        .unwrap();
        s.insert(
            EpisodeKind::Conversation,
            "user asked about the weather in Paris",
            0.5,
        )
        .unwrap();
        s.insert(
            EpisodeKind::Action,
            "rescheduled the advisor meeting to Tuesday",
            0.5,
        )
        .unwrap();
        let hits = s
            .retrieve("what did I say about the bedroom lights", 2)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].episode.text.contains("bedroom lights"));
    }

    #[test]
    fn tombstone_removes_from_retrieval() {
        let s = store();
        let id = s
            .insert(
                EpisodeKind::Conversation,
                "secret passphrase is hunter2",
                0.9,
            )
            .unwrap();
        s.tombstone(id).unwrap();
        let hits = s.retrieve("passphrase", 5).unwrap();
        assert!(hits.iter().all(|h| !h.episode.text.contains("hunter2")));
    }

    #[test]
    fn reinforce_caps_at_one() {
        let s = store();
        let id = s.insert(EpisodeKind::Conversation, "x", 0.9).unwrap();
        s.reinforce(id, 0.5).unwrap();
        let conn = s.conn.lock().unwrap();
        let sal: f64 = conn
            .query_row(
                "SELECT salience FROM episode WHERE id=?",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!((sal - 1.0).abs() < 1e-6);
    }
}
