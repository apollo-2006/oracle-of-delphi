//! Embedding trait + an offline, dependency-free default.
//!
//! `HashEmbedder` is a deterministic hashing bag-of-words embedder: it maps
//! tokens into a fixed-dim space with signed hashing and L2-normalizes. It is
//! NOT semantically strong — it exists so the memory stack runs with zero model
//! downloads in CI and the demo. Production swaps in a real model behind the
//! same trait (e.g. BGE-small via ONNX Runtime), and nothing else changes.

/// Embedding dimension. Real BGE-small is 384; the hash embedder uses the same
/// width so a later swap needs no schema migration.
pub const EMBED_DIM: usize = 384;

pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;

    fn embed_batch(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

pub struct HashEmbedder {
    dim: usize,
}

impl Default for HashEmbedder {
    fn default() -> Self {
        HashEmbedder { dim: EMBED_DIM }
    }
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        HashEmbedder { dim }
    }
}

fn token_hash(tok: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tok.hash(&mut h);
    h.finish()
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for raw in text.split(|c: char| !c.is_alphanumeric()) {
            if raw.is_empty() {
                continue;
            }
            let tok = raw.to_lowercase();
            let h = token_hash(&tok);
            let idx = (h % self.dim as u64) as usize;
            // Signed hashing: second bit picks the sign, reducing collisions bias.
            let sign = if (h >> 1) & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
        // L2 normalize so cosine == dot.
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::cosine;

    #[test]
    fn deterministic_and_normalized() {
        let e = HashEmbedder::default();
        let a = e.embed("check my calendar tomorrow");
        let b = e.embed("check my calendar tomorrow");
        assert_eq!(a, b);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn similar_text_scores_higher_than_unrelated() {
        let e = HashEmbedder::default();
        let q = e.embed("dim the bedroom lights");
        let near = e.embed("dim the bedroom lights please");
        let far = e.embed("what is the capital of France");
        assert!(cosine(&q, &near) > cosine(&q, &far));
    }
}
