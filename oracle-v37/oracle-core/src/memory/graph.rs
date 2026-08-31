//! Knowledge graph over the same SQLite connection (architecture §5.3).
//!
//! Bitemporal, provenanced property graph. The LLM never writes raw Cypher/SQL;
//! it goes through a constrained mini-language ([`KgQuery`]) that compiles to
//! parameterized SQL here. Unknown relations are rejected against a vocabulary
//! so the model gets corrected in-context instead of injecting arbitrary edges.

use rusqlite::{params, Connection};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub src: String,
    pub rel: String,
    pub dst: String,
}

/// The constrained query surface exposed to the model as the `kg.query` tool.
#[derive(Debug, Clone)]
pub enum KgQuery {
    /// All outgoing (rel, neighbor) for an entity, optional relation filter,
    /// bounded depth (1..=3).
    Neighbors {
        entity: String,
        rel: Option<String>,
        depth: u8,
    },
    /// Assert a new fact (T1, journaled). Resolves/creates nodes.
    Assert {
        subj: String,
        rel: String,
        obj: String,
        provenance: String,
    },
}

pub struct KnowledgeGraph {
    conn: Mutex<Connection>,
    /// Allowed relation vocabulary. Assertions/queries with unknown rels are
    /// rejected and the vocabulary is returned to teach the model.
    vocab: Vec<String>,
}

impl KnowledgeGraph {
    /// Share the store's connection file by opening the same path. In this
    /// reference build the graph opens its own connection to the same DB, which
    /// is safe under WAL for a single-writer process.
    pub fn open(path: &str, vocab: Vec<String>) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Schema is created by MemoryStore; ensure it exists if graph opens first.
        conn.execute_batch(super::store::graph_schema())?;
        Ok(KnowledgeGraph {
            conn: Mutex::new(conn),
            vocab,
        })
    }

    pub fn default_vocab() -> Vec<String> {
        [
            "member_of",
            "owns",
            "advisor",
            "email",
            "in_room",
            "attendee",
            "from",
            "works_on",
            "depends_on",
            "located_in",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn known_rel(&self, rel: &str) -> bool {
        self.vocab.iter().any(|v| v == rel)
    }

    /// Resolve an entity name to a node id via exact name or alias; create it as
    /// a generic node if absent (entity resolution §5.3, simplified — no
    /// embedding match in the offline build).
    fn resolve_or_create(conn: &Connection, name: &str) -> rusqlite::Result<i64> {
        // Alias first.
        if let Ok(id) = conn.query_row(
            "SELECT node_id FROM kg_alias WHERE alias = ?",
            params![name],
            |r| r.get::<_, i64>(0),
        ) {
            return Ok(id);
        }
        // Exact name.
        if let Ok(id) = conn.query_row(
            "SELECT id FROM kg_node WHERE name = ?",
            params![name],
            |r| r.get::<_, i64>(0),
        ) {
            return Ok(id);
        }
        conn.execute(
            "INSERT INTO kg_node(type, name) VALUES('entity', ?)",
            params![name],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Execute a query. Returns edges for `Neighbors`, or the asserted edge for
    /// `Assert`. `Err(String)` carries a model-facing repair message.
    pub fn query(&self, q: KgQuery) -> Result<Vec<Edge>, String> {
        match q {
            KgQuery::Assert {
                subj,
                rel,
                obj,
                provenance,
            } => {
                if !self.known_rel(&rel) {
                    return Err(format!(
                        "unknown relation '{rel}'. known relations: {}",
                        self.vocab.join(", ")
                    ));
                }
                let conn = self.conn.lock().unwrap();
                let s = Self::resolve_or_create(&conn, &subj).map_err(|e| e.to_string())?;
                let d = Self::resolve_or_create(&conn, &obj).map_err(|e| e.to_string())?;
                let now = chrono::Utc::now().timestamp();
                conn.execute(
                    "INSERT INTO kg_edge(src, rel, dst, t_valid_from, provenance) VALUES(?,?,?,?,?)",
                    params![s, rel, d, now, provenance],
                )
                .map_err(|e| e.to_string())?;
                Ok(vec![Edge {
                    src: subj,
                    rel,
                    dst: obj,
                }])
            }
            KgQuery::Neighbors { entity, rel, depth } => {
                let depth = depth.clamp(1, 3);
                if let Some(r) = &rel {
                    if !self.known_rel(r) {
                        return Err(format!(
                            "unknown relation '{r}'. known relations: {}",
                            self.vocab.join(", ")
                        ));
                    }
                }
                let conn = self.conn.lock().unwrap();
                let start = match conn.query_row(
                    "SELECT id FROM kg_node WHERE name = ?",
                    params![entity],
                    |r| r.get::<_, i64>(0),
                ) {
                    Ok(id) => id,
                    Err(_) => return Ok(vec![]), // unknown entity → empty, not error
                };
                // BFS to `depth`, collecting edges. Only currently-valid edges
                // (t_valid_to IS NULL) are traversed.
                let mut frontier = vec![start];
                let mut visited = std::collections::HashSet::new();
                visited.insert(start);
                let mut edges = Vec::new();
                for _ in 0..depth {
                    let mut next = Vec::new();
                    for node in &frontier {
                        let sql = "SELECT n1.name, e.rel, n2.name, e.dst \
                                   FROM kg_edge e \
                                   JOIN kg_node n1 ON n1.id = e.src \
                                   JOIN kg_node n2 ON n2.id = e.dst \
                                   WHERE e.src = ? AND e.t_valid_to IS NULL";
                        let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
                        let rows = stmt
                            .query_map(params![node], |r| {
                                Ok((
                                    r.get::<_, String>(0)?,
                                    r.get::<_, String>(1)?,
                                    r.get::<_, String>(2)?,
                                    r.get::<_, i64>(3)?,
                                ))
                            })
                            .map_err(|e| e.to_string())?;
                        for row in rows {
                            let (s, rl, d, dst_id) = row.map_err(|e| e.to_string())?;
                            if let Some(filter) = &rel {
                                if &rl != filter {
                                    continue;
                                }
                            }
                            edges.push(Edge {
                                src: s,
                                rel: rl,
                                dst: d,
                            });
                            if visited.insert(dst_id) {
                                next.push(dst_id);
                            }
                        }
                    }
                    frontier = next;
                    if frontier.is_empty() {
                        break;
                    }
                }
                Ok(edges)
            }
        }
    }

    /// Add an alias to an existing node (accumulates over time, §5.3).
    /// Where an edge came from, if it exists.
    ///
    /// Provenance is recorded on every assertion but deliberately not returned
    /// in [`Edge`]: that struct is what the planner sees, and widening it is a
    /// change to the tool contract. This accessor exists because the question
    /// "did this fact come from something the user said, or from a web page
    /// that was on screen?" has to be answerable — the consolidation pass marks
    /// screen-derived batches for exactly that reason.
    pub fn edge_provenance(&self, subj: &str, rel: &str, obj: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT e.provenance FROM kg_edge e \
             JOIN kg_node s ON s.id = e.src \
             JOIN kg_node d ON d.id = e.dst \
             WHERE s.name = ? AND e.rel = ? AND d.name = ? \
             ORDER BY e.id DESC LIMIT 1",
            params![subj, rel, obj],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    pub fn add_alias(&self, name: &str, alias: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let id = Self::resolve_or_create(&conn, name)?;
        conn.execute(
            "INSERT OR IGNORE INTO kg_alias(node_id, alias) VALUES(?, ?)",
            params![id, alias],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kg() -> KnowledgeGraph {
        // shared in-memory across the two connections isn't possible with plain
        // :memory:, so use a temp file path unique per test.
        let path = format!("/tmp/oracle-kg-test-{}.db", uuid::Uuid::new_v4());
        KnowledgeGraph::open(&path, KnowledgeGraph::default_vocab()).unwrap()
    }

    #[test]
    fn assert_then_neighbors() {
        let g = kg();
        g.query(KgQuery::Assert {
            subj: "User".into(),
            rel: "advisor".into(),
            obj: "Dr. Chen".into(),
            provenance: "ep:1".into(),
        })
        .unwrap();
        let edges = g
            .query(KgQuery::Neighbors {
                entity: "User".into(),
                rel: Some("advisor".into()),
                depth: 1,
            })
            .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].dst, "Dr. Chen");
    }

    #[test]
    fn provenance_is_readable_back_off_an_asserted_edge() {
        let g = kg();
        g.query(KgQuery::Assert {
            subj: "User".into(),
            rel: "advisor".into(),
            obj: "Dr. Chen".into(),
            provenance: "ep:4-9/screen".into(),
        })
        .unwrap();
        assert_eq!(
            g.edge_provenance("User", "advisor", "Dr. Chen").as_deref(),
            Some("ep:4-9/screen")
        );
        assert!(g.edge_provenance("User", "advisor", "Nobody").is_none());
    }

    #[test]
    fn unknown_relation_is_rejected_with_vocab() {
        let g = kg();
        let err = g
            .query(KgQuery::Assert {
                subj: "a".into(),
                rel: "frobnicates".into(),
                obj: "b".into(),
                provenance: String::new(),
            })
            .unwrap_err();
        assert!(err.contains("unknown relation"));
        assert!(err.contains("advisor")); // vocabulary is surfaced
    }

    #[test]
    fn alias_resolves_to_same_node() {
        let g = kg();
        g.query(KgQuery::Assert {
            subj: "Dr. Chen".into(),
            rel: "email".into(),
            obj: "chen@univ.edu".into(),
            provenance: String::new(),
        })
        .unwrap();
        g.add_alias("Dr. Chen", "my advisor").unwrap();
        // Assert against the alias; should attach to the same Dr. Chen node.
        g.query(KgQuery::Assert {
            subj: "my advisor".into(),
            rel: "owns".into(),
            obj: "Lab 5".into(),
            provenance: String::new(),
        })
        .unwrap();
        let edges = g
            .query(KgQuery::Neighbors {
                entity: "Dr. Chen".into(),
                rel: None,
                depth: 1,
            })
            .unwrap();
        // Both email and owns edges hang off the one node.
        assert_eq!(edges.len(), 2);
    }

    #[test]
    fn multi_hop_traversal() {
        let g = kg();
        for (s, r, o) in [("User", "owns", "Light1"), ("Light1", "in_room", "Bedroom")] {
            g.query(KgQuery::Assert {
                subj: s.into(),
                rel: r.into(),
                obj: o.into(),
                provenance: String::new(),
            })
            .unwrap();
        }
        let edges = g
            .query(KgQuery::Neighbors {
                entity: "User".into(),
                rel: None,
                depth: 2,
            })
            .unwrap();
        assert!(edges.iter().any(|e| e.dst == "Bedroom"));
    }
}
