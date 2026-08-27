//! Dual-layer persistent memory (architecture §5).
//!
//! One SQLite file holds three layers with transactional consistency:
//!   * episodic  — conversations/actions/observations, each with an embedding
//!   * vector    — embeddings live inline on the episode rows (cosine in SQL/Rust)
//!   * graph     — bitemporal, provenanced (subject,rel,object) triples
//!
//! Retrieval fuses vector similarity + keyword (FTS-lite) + graph expansion via
//! reciprocal-rank fusion (§5.2). Embeddings go through the [`Embedder`] trait;
//! a dependency-free hashing embedder ships as the offline default so the whole
//! stack builds and runs without a model download. Swap in BGE/MiniLM via ONNX
//! by implementing `Embedder`.

pub mod embed;
pub mod graph;
pub mod store;

pub use embed::{Embedder, HashEmbedder};
pub use graph::{Edge, KnowledgeGraph};
pub use store::{Episode, EpisodeKind, MemoryStore, RetrievedItem};

/// Cosine similarity between two equal-length vectors. Returns 0 for a zero
/// vector (avoids NaN) — a defensive default the retrieval ranker relies on.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Reciprocal-rank fusion of several ranked id lists (§5.2). `k` damps the
/// contribution of low ranks; 60 is the common default.
pub fn reciprocal_rank_fusion(lists: &[Vec<i64>], k: f32) -> Vec<i64> {
    use std::collections::HashMap;
    let mut score: HashMap<i64, f32> = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *score.entry(*id).or_insert(0.0) += 1.0 / (k + rank as f32 + 1.0);
        }
    }
    let mut ids: Vec<_> = score.into_iter().collect();
    ids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    ids.into_iter().map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basic() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0); // length mismatch
    }

    #[test]
    fn rrf_prefers_consistently_high_items() {
        // id 5 is near the top of both lists; id 9 only in one.
        let l1 = vec![5, 1, 2, 9];
        let l2 = vec![5, 3, 4];
        let fused = reciprocal_rank_fusion(&[l1, l2], 60.0);
        assert_eq!(fused[0], 5);
    }
}
