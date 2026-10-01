//! Direct memory search for the HUD (Ctrl+K).
//!
//! Asking the assistant "what was I reading on Tuesday" works, but it costs a
//! planner turn: load the 14B if it was idle-unloaded, plan, maybe call
//! `memory.recall`, compose a sentence. Search skips all of that. It runs the
//! same hybrid retrieval the planner's recall block uses and hands back the
//! rows themselves, so it answers in milliseconds with the planner unloaded,
//! and it shows *every* candidate rather than the one the model chose to
//! mention.
//!
//! An empty query is "what have I been looking at": the latest observations,
//! newest first.

use crate::memory::{Episode, EpisodeKind, MemoryStore};
use oracle_ipc::SearchHit;

/// How many hits a search returns. Enough to scan, few enough to read.
pub const SEARCH_LIMIT: usize = 20;

/// A row that placed on meaning alone must score at least this to be shown.
///
/// Retrieval always returns its top N, and a small embedder never scores
/// unrelated text near zero, so without a floor a query with no real match
/// still fills the panel with whatever was nearest. Calibrated on bge-small
/// with mean pooling, where a related row scored 0.56 and an unrelated one
/// 0.37 for the same query (score = 0.85 x cosine + 0.15 x salience). Rows that
/// share most of the query's words are kept regardless.
const MEANING_FLOOR: f32 = 0.45;
const WORDS_FLOOR: f32 = 0.5;

/// Repeat sightings of one window inside this span collapse into the newest.
/// The sampler stores a new observation whenever the pixels change enough, so
/// a video or a scrolling page yields a run of near-identical rows.
const COLLAPSE_SECS: i64 = 15 * 60;

/// Run a search and shape the result for the HUD.
///
/// Returns the hits and, when the list is empty, a note saying why, so an
/// empty panel is never ambiguous between "nothing matched" and "nothing is
/// being recorded".
pub fn search(
    store: &MemoryStore,
    query: &str,
    ambient_on: bool,
) -> anyhow::Result<(Vec<SearchHit>, Option<String>)> {
    let query = query.trim();
    let hits: Vec<SearchHit> = if query.is_empty() {
        store
            .recent_observations(SEARCH_LIMIT, 0)?
            .into_iter()
            .map(|e| hit(e, 0.0))
            .collect()
    } else {
        store
            .retrieve(query, SEARCH_LIMIT)?
            .into_iter()
            .filter(|r| r.keyword >= WORDS_FLOOR || r.score >= MEANING_FLOOR)
            .map(|r| hit(r.episode, r.score))
            .collect()
    };
    let hits = collapse_repeats(hits);

    let note = match (hits.is_empty(), query.is_empty(), ambient_on) {
        (false, ..) => None,
        (true, true, true) => Some(
            "Nothing seen yet. The screen is sampled every so often while you work; \
             observations appear once the vision model has read them."
                .to_string(),
        ),
        (true, _, false) => Some(
            "The ambient index is off, so only conversations are searchable. \
             Turn on [ambient] in oracle.toml to search what has been on screen."
                .to_string(),
        ),
        (true, false, true) => Some(format!(
            "Nothing in memory matches \u{201c}{query}\u{201d}."
        )),
    };
    Ok((hits, note))
}

/// Drop an observation when a better-placed one (earlier in the list) has the
/// same window title within [`COLLAPSE_SECS`]. Order is preserved, so the
/// survivor is the most relevant (search) or the newest (timeline). Untitled
/// observations and other kinds are never collapsed: without a title there is
/// nothing to say two rows are the same thing.
fn collapse_repeats(hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut kept: Vec<SearchHit> = Vec::with_capacity(hits.len());
    for h in hits {
        let dup = h.kind == "observation"
            && h.title.is_some()
            && kept.iter().any(|k| {
                k.kind == h.kind
                    && k.title == h.title
                    && (k.t_unix - h.t_unix).abs() <= COLLAPSE_SECS
            });
        if !dup {
            kept.push(h);
        }
    }
    kept
}

fn hit(e: Episode, score: f32) -> SearchHit {
    let kind = match e.kind {
        EpisodeKind::Conversation => "conversation",
        EpisodeKind::Action => "action",
        EpisodeKind::Observation => "observation",
    };
    let (title, text) = match e.kind {
        EpisodeKind::Observation => match crate::ambient::parse_observation(&e.text) {
            Some((title, summary)) => (title.map(str::to_string), summary.to_string()),
            None => (None, e.text),
        },
        _ => (None, e.text),
    };
    SearchHit {
        kind: kind.into(),
        title,
        text,
        t_unix: e.t_unix,
        // A NaN would serialize as null and break the HUD's sort; see the
        // `total_cmp` note in MemoryStore::retrieve for where one comes from.
        score: if score.is_finite() { score } else { 0.0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambient::render_observation;
    use crate::memory::HashEmbedder;

    fn store() -> MemoryStore {
        MemoryStore::open(":memory:", Box::new(HashEmbedder::default())).unwrap()
    }

    #[test]
    fn a_query_finds_an_observation_and_splits_out_its_window() {
        let s = store();
        s.insert(
            EpisodeKind::Observation,
            &render_observation("docs.rs — tokio", "tokio::select! macro documentation"),
            0.25,
        )
        .unwrap();
        s.insert(EpisodeKind::Conversation, "dim the bedroom lights", 0.6)
            .unwrap();

        let (hits, note) = search(&s, "tokio select", true).unwrap();
        assert_eq!(note, None);
        let top = &hits[0];
        assert_eq!(top.kind, "observation");
        assert_eq!(top.title.as_deref(), Some("docs.rs — tokio"));
        assert_eq!(top.text, "tokio::select! macro documentation");
    }

    #[test]
    fn an_empty_query_is_the_recent_screen_timeline() {
        let s = store();
        s.insert(EpisodeKind::Conversation, "hello", 0.6).unwrap();
        s.insert(
            EpisodeKind::Observation,
            &render_observation("a", "first"),
            0.25,
        )
        .unwrap();
        let (hits, _) = search(&s, "   ", true).unwrap();
        assert_eq!(hits.len(), 1, "conversations are not part of the timeline");
        assert_eq!(hits[0].text, "first");
    }

    #[test]
    fn an_empty_result_says_why() {
        let s = store();
        let (_, note) = search(&s, "", false).unwrap();
        assert!(note.unwrap().contains("ambient index is off"));
        let (_, note) = search(&s, "", true).unwrap();
        assert!(note.unwrap().contains("Nothing seen yet"));
        let (_, note) = search(&s, "zebra", true).unwrap();
        assert!(note.unwrap().contains("zebra"));
    }

    fn obs(title: Option<&str>, t: i64) -> SearchHit {
        SearchHit {
            kind: "observation".into(),
            title: title.map(str::to_string),
            text: format!("at {t}"),
            t_unix: t,
            score: 0.0,
        }
    }

    #[test]
    fn a_run_of_one_window_collapses_but_a_later_visit_does_not() {
        let hits = vec![
            obs(Some("YouTube"), 1_000),
            obs(Some("YouTube"), 990), // same sitting: collapsed
            obs(Some("main.rs"), 980),
            obs(None, 970), // untitled: never collapsed
            obs(None, 960),
            obs(Some("YouTube"), 1_000 - 3_600), // an hour earlier: kept
        ];
        let out: Vec<i64> = collapse_repeats(hits).iter().map(|h| h.t_unix).collect();
        assert_eq!(out, vec![1_000, 980, 970, 960, -2_600]);
    }

    /// Embeds by a fixed table, so similarity is chosen rather than hashed.
    struct TableEmbedder;
    impl crate::memory::Embedder for TableEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            let t = text.to_lowercase();
            // Two axes: "comedy" and "code". Everything has a little of both,
            // like a real small embedder, so unrelated text never scores 0.
            // Off-axis cosine 0.39, about what bge-small gave unrelated text live.
            let v = if t.contains("comedian") || t.contains("stand-up") {
                vec![0.98, 0.2]
            } else if t.contains("compiler") || t.contains("borrow") {
                vec![0.2, 0.98]
            } else {
                vec![0.707, 0.707]
            };
            Ok(v)
        }
        fn dim(&self) -> usize {
            2
        }
        fn id(&self) -> &str {
            "table"
        }
    }

    #[test]
    fn a_query_with_no_real_match_shows_nothing_rather_than_the_nearest_rows() {
        let s = MemoryStore::open(":memory:", Box::new(TableEmbedder)).unwrap();
        s.insert(
            EpisodeKind::Observation,
            &render_observation("YouTube", "a stand-up set"),
            0.25,
        )
        .unwrap();
        // Off-topic: score ~0.37, below the floor, and no shared words.
        let (hits, note) = search(&s, "borrow checker", true).unwrap();
        assert!(
            hits.is_empty(),
            "off-topic rows must not fill the panel: {hits:?}"
        );
        assert!(note.is_some());
        // On-topic by meaning alone (no shared words): shown.
        let (hits, _) = search(&s, "that comedian", true).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn forgotten_memories_do_not_come_back_through_search() {
        let s = store();
        let id = s
            .insert(EpisodeKind::Conversation, "my passphrase is swordfish", 0.6)
            .unwrap();
        s.tombstone(id).unwrap();
        let (hits, _) = search(&s, "passphrase swordfish", true).unwrap();
        assert!(hits.is_empty());
    }
}
