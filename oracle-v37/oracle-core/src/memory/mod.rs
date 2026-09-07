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

pub use embed::{Embedder, HashEmbedder, HttpEmbedder, EMBED_DIM, HASH_SPACE};
pub use graph::{Edge, KnowledgeGraph};
pub use store::{Episode, EpisodeKind, MemoryStore, RetrievedItem};

/// Cosine similarity between two equal-length vectors.
///
/// Returns 0 rather than a non-finite number for a length mismatch, a zero
/// vector, or an input carrying a NaN or an infinity — a defensive default the
/// retrieval ranker relies on. It relies on it more than "avoids a panic"
/// suggests: a NaN score is not merely unsortable, it *wins*. Every comparison
/// against NaN is false, so it slips past the `recall_min_score` floor, and
/// `f32::total_cmp` ranks it above every real number — a garbage memory would
/// land at the top of the prompt's recall block. Vectors arrive from an
/// embedding sidecar that can be mid-restart or serving a different model, so
/// this is the boundary where that has to stop.
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
    finite_ratio(dot, na, nb)
}

/// The shared tail of both cosines: `dot / (|a| |b|)`, or 0 if that is not a
/// real number.
fn finite_ratio(dot: f32, na: f32, nb: f32) -> f32 {
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    let c = dot / (na.sqrt() * nb.sqrt());
    if c.is_finite() {
        c
    } else {
        0.0
    }
}

/// [`cosine`] against an embedding still in the little-endian byte form the
/// store keeps it in.
///
/// Same contract as `cosine`, including 0 for a length mismatch or a zero
/// vector — the two are held to that by `cosine_blob_agrees_with_cosine`.
/// Retrieval scans every row on every recall, so decoding each embedding into a
/// `Vec<f32>` first means one heap allocation per episode per turn to produce
/// numbers that are summed into a single float and dropped.
pub(crate) fn cosine_blob(a: &[f32], blob: &[u8]) -> f32 {
    let (chunks, _remainder) = blob.as_chunks::<4>();
    if a.len() != chunks.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, c) in a.iter().zip(chunks) {
        let y = f32::from_le_bytes(*c);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    finite_ratio(dot, na, nb)
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
    // `total_cmp`: RRF scores are well-formed here, but this sort sits on the
    // per-turn recall path and a panicking comparator is not the way to find
    // out otherwise.
    ids.sort_by(|a, b| b.1.total_cmp(&a.1));
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

    /// The byte-form cosine is an optimization, so it has to be provably the
    /// same function — including the two defensive zeros.
    #[test]
    fn cosine_blob_agrees_with_cosine() {
        fn bytes(v: &[f32]) -> Vec<u8> {
            v.iter().flat_map(|x| x.to_le_bytes()).collect()
        }
        for (a, b) in [
            (vec![1.0, 0.0], vec![1.0, 0.0]),
            (vec![1.0, 0.0], vec![0.0, 1.0]),
            (vec![0.3, -0.7, 0.11], vec![-0.2, 0.9, 0.4]),
            (vec![0.0, 0.0], vec![1.0, 1.0]), // zero vector
            (vec![1.0], vec![1.0, 2.0]),      // length mismatch
        ] {
            assert_eq!(
                cosine_blob(&a, &bytes(&b)),
                cosine(&a, &b),
                "disagreed on {a:?} vs {b:?}"
            );
        }
    }

    /// A vector carrying a NaN or an infinity scores 0, not a non-finite number.
    ///
    /// The old code reached `partial_cmp().unwrap()` and panicked the recall
    /// that now runs every turn. Merely not panicking would not be enough:
    /// `total_cmp` ranks NaN above every real number and `NaN < min_score` is
    /// false, so a poisoned score would have taken the top of the recall block.
    #[test]
    fn a_non_finite_embedding_scores_zero() {
        let good = [1.0f32, 0.0, 0.0];
        for bad in [
            vec![f32::NAN, 0.0, 0.0],
            vec![f32::INFINITY, 0.0, 0.0],
            vec![f32::NEG_INFINITY, 1.0, 0.0],
        ] {
            let bytes: Vec<u8> = bad.iter().flat_map(|x| x.to_le_bytes()).collect();
            assert_eq!(cosine(&good, &bad), 0.0, "cosine on {bad:?}");
            assert_eq!(cosine(&bad, &good), 0.0, "cosine on {bad:?} (reversed)");
            assert_eq!(cosine_blob(&good, &bytes), 0.0, "cosine_blob on {bad:?}");
        }
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
