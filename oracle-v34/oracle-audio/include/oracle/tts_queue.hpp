// TTS output flow control + barge-in flush (architecture §1.4, §1.2.2).
//
// Models the synthesis-ahead buffer: keep 1.5–3s of audio queued, synthesize
// eagerly below the low-water mark, idle above the high-water mark, and on
// barge-in flush immediately while retaining a short window for false-barge
// resume. Sample-accurate `played_samples` is what lets core truncate the
// assistant turn to exactly what was heard.
#pragma once
#include <cstdint>
#include <deque>
#include <string>

namespace oracle {

struct TtsClause {
    std::string text;
    uint64_t samples;      // synthesized length
    uint64_t start_sample; // cumulative position at start of this clause
};

enum class TtsDecision { Synthesize, Idle };

class TtsQueue {
public:
    TtsQueue(uint32_t sample_rate = 24000,
             double low_water_s = 1.5,
             double high_water_s = 3.0)
        : sr_(sample_rate),
          low_water_(static_cast<uint64_t>(low_water_s * sample_rate)),
          high_water_(static_cast<uint64_t>(high_water_s * sample_rate)) {}

    // Enqueue a synthesized clause.
    void push_clause(const std::string& text, uint64_t samples) {
        uint64_t start = produced_;
        clauses_.push_back({text, samples, start});
        produced_ += samples;
    }

    // Advance the play cursor by `n` samples (called by the output callback).
    void advance_play(uint64_t n) {
        played_ = played_ + n;
        if (played_ > produced_) played_ = produced_;
    }

    // How much synthesized-but-unplayed audio is buffered.
    uint64_t buffered() const { return produced_ - played_; }

    // Flow-control decision for the synthesis worker.
    TtsDecision should_synthesize() const {
        if (buffered() < low_water_) return TtsDecision::Synthesize;
        if (buffered() >= high_water_) return TtsDecision::Idle;
        return TtsDecision::Synthesize;  // in-band: keep filling toward high-water
    }

    // Barge-in: stop playback now. Returns how many samples were actually
    // played (heard_upto). Retains clauses for the false-barge resume window.
    uint64_t barge_in() {
        uint64_t heard = played_;
        // Keep the clause table (for resume + core's truncation mapping), but
        // stop producing/playing: future advance calls are ignored until reset.
        frozen_ = true;
        return heard;
    }

    // Resume after a false barge-in from a given sample (§1.2.4).
    void resume() { frozen_ = false; }

    bool frozen() const { return frozen_; }
    uint64_t played_samples() const { return played_; }
    uint64_t produced_samples() const { return produced_; }
    size_t clause_count() const { return clauses_.size(); }

    // Map a played-sample count back to a text character offset at a clause
    // boundary (mirror of core's ChunkTable; kept here for the audio process's
    // own bookkeeping).
    size_t text_offset_for_sample(uint64_t heard_upto) const {
        size_t chars = 0;
        for (const auto& c : clauses_) {
            if (c.start_sample + c.samples <= heard_upto) {
                chars += c.text.size();
            } else {
                break;
            }
        }
        return chars;
    }

private:
    uint32_t sr_;
    uint64_t low_water_;
    uint64_t high_water_;
    uint64_t produced_ = 0;
    uint64_t played_ = 0;
    bool frozen_ = false;
    std::deque<TtsClause> clauses_;
};

}  // namespace oracle
