//! Embedding trait, the offline default, and the llama.cpp embedding sidecar.
//!
//! `HashEmbedder` is a deterministic hashing bag-of-words embedder: it maps
//! tokens into a fixed-dim space with signed hashing and L2-normalizes. It is
//! NOT semantically strong — it exists so the memory stack runs with zero model
//! downloads in CI and the demo. It matches on tokens, so "the borrow checker
//! complaint in dispatch.rs" does not retrieve "lifetime error in the
//! dispatcher"; that is the ceiling this module exists to lift.
//!
//! [`HttpEmbedder`] is the real one: BGE-small served by a llama.cpp sidecar
//! (`--embedding --pooling mean`) on its own port, reached over the OpenAI
//! `/v1/embeddings` shape. It is a third supervised child rather than an
//! in-process ONNX Runtime, which keeps the native dependency surface of
//! `oracle-core` at zero and gives the embedder restart-on-crash and idle
//! management from infrastructure that already exists.
//!
//! ## Vector spaces, and why 384 == 384 is not enough
//!
//! Both embedders produce 384-dim unit vectors, so a row written by one and a
//! row written by the other are indistinguishable *as data*. They are not
//! comparable as *meaning*: cosine between a hashed vector and a BGE vector is
//! noise that happens to fall in [-1, 1]. Retrieval would keep working, keep
//! returning results, and keep being wrong — the worst failure shape there is.
//!
//! So every embedder names its space via [`Embedder::id`], the store records
//! that id alongside each row, and vector scoring only ever compares within one
//! space. Switching embedders does not corrupt history; it makes the old rows
//! vector-invisible until something re-embeds them, while keyword retrieval
//! (which is space-independent) keeps finding them in the meantime.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;

/// Embedding dimension. Real BGE-small is 384; the hash embedder uses the same
/// width so a later swap needs no *schema* change (it does need a re-embed —
/// see the module docs).
pub const EMBED_DIM: usize = 384;

/// The vector space id recorded for rows written by [`HashEmbedder`].
pub const HASH_SPACE: &str = "hash-384";

pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;

    /// Names the vector space this embedder produces. Rows are tagged with it,
    /// and cosine is only ever taken between rows sharing it.
    fn id(&self) -> &str;

    /// Embed one string.
    ///
    /// Fallible because the real implementation is a network call to a sidecar
    /// that can be down, still loading, or serving the wrong model. The failure
    /// must reach the caller: silently substituting a different embedder would
    /// write rows into the wrong space, which is the corruption this module is
    /// built to prevent.
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;

    /// Embed many strings. Overridden by backends with a real batch endpoint —
    /// the consolidation pass embeds hundreds of episodes at once, and one
    /// round trip per episode would dominate its runtime.
    fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
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

    fn id(&self) -> &str {
        HASH_SPACE
    }

    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
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
        Ok(l2_normalize(v))
    }
}

/// L2 normalize so cosine == dot. A zero vector is left alone rather than
/// producing NaNs — empty or punctuation-only text is legal input.
fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

// ---------------------------------------------------------------------------
// The llama.cpp embedding sidecar
// ---------------------------------------------------------------------------

/// One unit of work for the embedding worker thread.
struct Job {
    texts: Vec<String>,
    reply: Sender<anyhow::Result<Vec<Vec<f32>>>>,
}

/// BGE-small (or any embedding GGUF) served by a llama.cpp sidecar.
///
/// ## Why a worker thread
///
/// [`Embedder`] is synchronous, and it is called from inside `MemoryStore`,
/// which is called from async code. Making the trait async would push `.await`
/// through the store and every memory tool for one HTTP call — a large ripple
/// for a small need. Instead the async is confined here: a dedicated OS thread
/// owns a current-thread runtime, and `embed` hands it work over a channel and
/// blocks on the answer.
///
/// That does block a Tokio worker for the duration of the call. It is a
/// deliberate, bounded trade: the sidecar is on loopback with a ~30M model, so a
/// short query is single-digit milliseconds, and the alternative costs far more
/// in ceremony than it saves in latency. If this ever shows up in the latency
/// budget, the fix is to make the *store* async, not to paper over it here.
pub struct HttpEmbedder {
    id: String,
    dim: usize,
    tx: Sender<Job>,
    /// Repeat text is common — `reinforce` re-embeds an exact repeat, and the
    /// ambient index sees the same window title over and over. A small bounded
    /// cache removes most of that traffic.
    cache: Mutex<Cache>,
}

/// A bounded FIFO cache. Not an LRU: FIFO needs no bookkeeping on read, and at
/// this size the hit-rate difference does not pay for a dependency.
struct Cache {
    map: HashMap<String, Vec<f32>>,
    order: VecDeque<String>,
    cap: usize,
}

impl Cache {
    fn new(cap: usize) -> Self {
        Cache {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn get(&self, k: &str) -> Option<Vec<f32>> {
        self.map.get(k).cloned()
    }

    fn put(&mut self, k: String, v: Vec<f32>) {
        if self.cap == 0 || self.map.contains_key(&k) {
            return;
        }
        if self.map.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.order.push_back(k.clone());
        self.map.insert(k, v);
    }
}

impl HttpEmbedder {
    /// `base_url` is the sidecar root (e.g. `http://127.0.0.1:8082`); `model` is
    /// the name reported to the server and used as the vector-space id.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, dim: usize) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        let model = model.into();
        let (tx, rx) = channel::<Job>();
        let url = format!("{base}/v1/embeddings");
        let model_for_worker = model.clone();

        // The worker owns its own single-threaded runtime, so it is independent
        // of whatever runtime the caller happens to be on (including none, as in
        // the REPL and tests).
        std::thread::Builder::new()
            .name("oracle-embed".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(error = %e, "[embed] worker runtime failed to start");
                        return;
                    }
                };
                let client = reqwest::Client::new();
                // Ends when every sender is dropped, i.e. the embedder is gone.
                while let Ok(job) = rx.recv() {
                    let result = rt.block_on(fetch(&client, &url, &model_for_worker, &job.texts));
                    // A dropped receiver means the caller gave up; not an error.
                    let _ = job.reply.send(result);
                }
            })
            .expect("spawning the embedding worker thread");

        HttpEmbedder {
            id: model,
            dim,
            tx,
            cache: Mutex::new(Cache::new(512)),
        }
    }

    /// Send texts to the worker and wait for vectors.
    fn request(&self, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        let (reply, rx) = channel();
        self.tx
            .send(Job { texts, reply })
            .map_err(|_| anyhow::anyhow!("the embedding worker thread is gone"))?;
        let vectors = rx
            .recv()
            .map_err(|_| anyhow::anyhow!("the embedding worker dropped the request"))??;
        for v in &vectors {
            if v.len() != self.dim {
                // A wrong-width vector means the sidecar is serving a different
                // model than configured (bge-base is 768). Writing it would put
                // two widths in one column and make cosine a length mismatch
                // rather than a similarity, so refuse.
                anyhow::bail!(
                    "the embedding sidecar returned {} dims, expected {} — is it serving {}?",
                    v.len(),
                    self.dim,
                    self.id
                );
            }
        }
        Ok(vectors)
    }
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    texts: &[String],
) -> anyhow::Result<Vec<Vec<f32>>> {
    let body = serde_json::json!({ "model": model, "input": texts });
    let resp = client.post(url).json(&body).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("embedding sidecar returned {status}: {text}");
    }
    let parsed: EmbeddingResponse = resp.json().await?;
    if parsed.data.len() != texts.len() {
        anyhow::bail!(
            "embedding sidecar returned {} vectors for {} inputs",
            parsed.data.len(),
            texts.len()
        );
    }
    // The server is asked for one vector per input in input order; sort by index
    // anyway, because relying on response ordering is the kind of assumption
    // that holds until it silently does not.
    let mut data = parsed.data;
    data.sort_by_key(|d| d.index);
    Ok(data
        .into_iter()
        .map(|d| l2_normalize(d.embedding))
        .collect())
}

#[derive(serde::Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(serde::Deserialize)]
struct EmbeddingDatum {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

impl Embedder for HttpEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        if let Some(hit) = self.cache.lock().unwrap().get(text) {
            return Ok(hit);
        }
        let mut out = self.request(vec![text.to_string()])?;
        let v = out
            .pop()
            .ok_or_else(|| anyhow::anyhow!("no embedding returned"))?;
        self.cache.lock().unwrap().put(text.to_string(), v.clone());
        Ok(v)
    }

    fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // One round trip for everything not already cached, then reassemble in
        // the caller's order.
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        let mut misses = Vec::new();
        let mut miss_idx = Vec::new();
        {
            let cache = self.cache.lock().unwrap();
            for (i, t) in texts.iter().enumerate() {
                match cache.get(t) {
                    Some(v) => out[i] = Some(v),
                    None => {
                        misses.push(t.to_string());
                        miss_idx.push(i);
                    }
                }
            }
        }
        if !misses.is_empty() {
            let fetched = self.request(misses.clone())?;
            let mut cache = self.cache.lock().unwrap();
            for ((i, text), v) in miss_idx.into_iter().zip(misses).zip(fetched) {
                cache.put(text, v.clone());
                out[i] = Some(v);
            }
        }
        Ok(out.into_iter().map(|v| v.unwrap_or_default()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::cosine;

    #[test]
    fn deterministic_and_normalized() {
        let e = HashEmbedder::default();
        let a = e.embed("check my calendar tomorrow").unwrap();
        let b = e.embed("check my calendar tomorrow").unwrap();
        assert_eq!(a, b);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn similar_text_scores_higher_than_unrelated() {
        let e = HashEmbedder::default();
        let q = e.embed("dim the bedroom lights").unwrap();
        let near = e.embed("dim the bedroom lights please").unwrap();
        let far = e.embed("what is the capital of France").unwrap();
        assert!(cosine(&q, &near) > cosine(&q, &far));
    }

    #[test]
    fn empty_text_does_not_produce_nans() {
        let e = HashEmbedder::default();
        let v = e.embed("   ...   ").unwrap();
        assert!(v.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn the_hash_embedder_names_its_space() {
        assert_eq!(HashEmbedder::default().id(), HASH_SPACE);
    }

    #[test]
    fn a_dead_sidecar_is_an_error_not_a_silent_fallback() {
        // Port 1 is never a llama.cpp server. The contract under test is that
        // this surfaces as Err: a fallback to hashed vectors here would write
        // rows into the wrong vector space and quietly corrupt retrieval.
        let e = HttpEmbedder::new("http://127.0.0.1:1", "bge-small-en-v1.5", EMBED_DIM);
        assert!(e.embed("anything").is_err());
    }

    #[test]
    fn the_cache_evicts_in_fifo_order_and_stays_bounded() {
        let mut c = Cache::new(2);
        c.put("a".into(), vec![1.0]);
        c.put("b".into(), vec![2.0]);
        c.put("c".into(), vec![3.0]);
        assert_eq!(c.map.len(), 2);
        assert!(c.get("a").is_none(), "oldest is evicted");
        assert!(c.get("b").is_some());
        assert!(c.get("c").is_some());
    }

    #[test]
    fn a_zero_capacity_cache_stores_nothing() {
        let mut c = Cache::new(0);
        c.put("a".into(), vec![1.0]);
        assert!(c.get("a").is_none());
    }

    #[test]
    fn an_empty_batch_makes_no_request() {
        // Must not touch the network — a dead address proves it short-circuits.
        let e = HttpEmbedder::new("http://127.0.0.1:1", "bge-small-en-v1.5", EMBED_DIM);
        assert_eq!(e.embed_batch(&[]).unwrap().len(), 0);
    }
}
