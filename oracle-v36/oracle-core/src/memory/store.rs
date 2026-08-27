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
        Ok(MemoryStore {
            conn: Mutex::new(conn),
            embedder,
        })
    }

    /// Insert an episode, embedding its text. Returns the new row id.
    pub fn insert(&self, kind: EpisodeKind, text: &str, salience: f32) -> anyhow::Result<i64> {
        let emb = self.embedder.embed(text);
        let blob = f32_to_bytes(&emb);
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO episode(kind, text, t_unix, salience, embedding) VALUES(?,?,?,?,?)",
            params![kind.as_str(), text, now, salience, blob],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Hybrid retrieval: vector top-N ∪ keyword top-N, fused by RRF, re-scored by
    /// cosine, salience-weighted, top-`limit` returned. Dates are the caller's
    /// job to stamp into the prompt.
    pub fn retrieve(&self, query: &str, limit: usize) -> anyhow::Result<Vec<RetrievedItem>> {
        let q_emb = self.embedder.embed(query);
        let conn = self.conn.lock().unwrap();

        // Pull candidate rows (in a real deployment this is an HNSW/FTS prefilter;
        // for the reference impl we scan, which is fine at personal scale and
        // keeps the dependency surface tiny).
        let mut stmt =
            conn.prepare("SELECT id, kind, text, t_unix, salience, embedding FROM episode")?;
        let rows = stmt.query_map([], |r| {
            let blob: Vec<u8> = r.get(5)?;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, f64>(4)? as f32,
                bytes_to_f32(&blob),
            ))
        })?;

        let mut cands = Vec::new();
        for row in rows {
            let (id, kind, text, t_unix, salience, emb) = row?;
            let vscore = cosine(&q_emb, &emb);
            let kscore = keyword_overlap(query, &text);
            cands.push((id, kind, text, t_unix, salience, vscore, kscore));
        }

        // Rank lists for RRF.
        let mut by_vec: Vec<_> = cands.iter().map(|c| (c.0, c.5)).collect();
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
            "UPDATE episode SET text='[forgotten]', salience=0.0, embedding=? WHERE id=?",
            params![zero, id],
        )?;
        Ok(())
    }

    pub fn count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))?)
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS episode (
    id        INTEGER PRIMARY KEY,
    kind      TEXT NOT NULL,
    text      TEXT NOT NULL,
    t_unix    INTEGER NOT NULL,
    salience  REAL NOT NULL DEFAULT 0.5,
    embedding BLOB NOT NULL
);
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
