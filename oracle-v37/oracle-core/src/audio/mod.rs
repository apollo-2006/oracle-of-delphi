//! Core-side audio helpers (architecture §1.2.3, §1.4).
//!
//! The hard real-time capture/VAD/vocoder code is the C++ `oracle-audio`
//! process. But two pieces of logic live in core because they sit at the
//! LLM↔TTS boundary and are pure/testable:
//!   * the **clause chunker** that turns a token stream into speakable units;
//!   * the **chunk table** that maps played audio samples back to a text offset,
//!     so a barge-in truncates the assistant's turn to exactly what was heard.

/// Splits a growing text buffer into speakable clauses. Feed it token deltas;
/// it yields clauses as they become emittable (architecture §1.4).
#[derive(Default)]
pub struct ClauseChunker {
    buf: String,
    first_emitted: bool,
}

impl ClauseChunker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push text; return any clauses now ready to synthesize.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        while let Some(clause) = self.take_ready() {
            out.push(clause);
        }
        out
    }

    /// Flush the remainder (end of generation) as a final clause if non-empty.
    pub fn flush(&mut self) -> Option<String> {
        let s = self.buf.trim();
        if s.is_empty() {
            None
        } else {
            let out = s.to_string();
            self.buf.clear();
            Some(out)
        }
    }

    fn take_ready(&mut self) -> Option<String> {
        // Find the earliest boundary that satisfies the emit rules.
        // Sentence punctuation always emits; clause punctuation emits only if
        // the accumulated clause has >= min_words. The first clause is allowed
        // to be short (>=2 words) to minimize time-to-first-audio.
        let min_words = if self.first_emitted { 4 } else { 2 };
        let bytes = self.buf.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            let is_sentence = matches!(b, b'.' | b'!' | b'?');
            let is_clause = matches!(b, b',' | b';' | b':');
            if !(is_sentence || is_clause) {
                continue;
            }
            // Don't split a decimal like "3.5" or an ellipsis mid-run.
            if is_sentence && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                continue;
            }
            let candidate = self.buf[..=i].trim();
            let words = candidate.split_whitespace().count();
            if is_sentence || (is_clause && words >= min_words) {
                let clause = candidate.to_string();
                // advance buffer past this boundary
                self.buf = self.buf[i + 1..].to_string();
                self.first_emitted = true;
                return Some(clause);
            }
        }
        None
    }
}

/// Maps synthesized audio back to text. Each synthesized clause contributes an
/// entry (cumulative sample range ↔ text length). On barge-in with a
/// `heard_upto_sample`, [`text_offset_for_sample`] returns how many characters
/// of the assistant's turn were actually voiced (architecture §1.2.3).
#[derive(Default)]
pub struct ChunkTable {
    /// (cumulative_end_sample, cumulative_end_char)
    entries: Vec<(u64, usize)>,
    total_samples: u64,
    total_chars: usize,
}

impl ChunkTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a synthesized clause: its text and how many samples it produced.
    pub fn record(&mut self, text: &str, samples: u64) {
        self.total_samples += samples;
        self.total_chars += text.len();
        self.entries.push((self.total_samples, self.total_chars));
    }

    /// Given how many samples actually reached the speaker, return the number of
    /// characters of the assistant turn that were heard (rounded to a clause
    /// boundary — we never claim a partially-played clause as fully spoken).
    pub fn text_offset_for_sample(&self, heard_upto_sample: u64) -> usize {
        let mut chars = 0;
        for (end_sample, end_char) in &self.entries {
            if *end_sample <= heard_upto_sample {
                chars = *end_char;
            } else {
                break;
            }
        }
        chars
    }

    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_clause_can_be_short() {
        let mut c = ClauseChunker::new();
        // "On it." is a sentence → emits immediately.
        let out = c.push("On it. ");
        assert_eq!(out, vec!["On it."]);
    }

    #[test]
    fn clause_comma_needs_min_words_after_first() {
        let mut c = ClauseChunker::new();
        c.push("Okay. "); // first sentence emitted, first_emitted=true
        let out = c.push("yes, "); // "yes," only 1 word → not emitted on comma
        assert!(out.is_empty());
        let out2 = c.push("that works for me, next. ");
        // now a longer clause + sentence
        assert!(out2.iter().any(|s| s.contains("that works for me")));
    }

    #[test]
    fn does_not_split_decimals() {
        let mut c = ClauseChunker::new();
        let out = c.push("It is 3.5 meters. ");
        assert_eq!(out, vec!["It is 3.5 meters."]);
    }

    #[test]
    fn flush_returns_remainder() {
        let mut c = ClauseChunker::new();
        c.push("no punctuation yet");
        assert_eq!(c.flush().as_deref(), Some("no punctuation yet"));
        assert!(c.flush().is_none());
    }

    #[test]
    fn chunk_table_maps_sample_to_clause_boundary() {
        let mut t = ChunkTable::new();
        t.record("Hello there.", 16000); // 12 chars, ends at 16000
        t.record(" I drafted a reply.", 24000); // ends at 40000
        t.record(" Lights are dimmed.", 20000); // ends at 60000

        // Heard 30000 samples: only the first clause fully played.
        assert_eq!(t.text_offset_for_sample(30000), 12);
        // Heard 45000: first two clauses.
        assert_eq!(t.text_offset_for_sample(45000), 12 + 19);
        // Heard everything.
        assert_eq!(t.text_offset_for_sample(60000), t.total_chars);
        // Heard nothing.
        assert_eq!(t.text_offset_for_sample(0), 0);
    }
}
