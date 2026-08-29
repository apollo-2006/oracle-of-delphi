// VAD hysteresis + barge-in state machine (architecture §1.2).
//
// This models the control logic around the neural VAD (Silero/ONNX in prod).
// It takes per-hop speech probabilities and emits state transitions with the
// fast-onset, adaptive-hangover, and barge-in behavior from the design. The
// neural net itself is injected as a probability stream so this logic is
// testable without a model.
#pragma once
#include <cstdint>

namespace oracle {

enum class VadState { Silence, Speech };

enum class VadEvent {
    None,
    SpeechStart,  // onset detected (fires the barge-in path if TTS is playing)
    SpeechEnd,    // endpoint after hangover
};

struct VadConfig {
    float onset_fast = 0.85f;    // single hop above this → immediate onset
    float onset_slow = 0.60f;    // two hops above this → onset
    float release = 0.35f;       // below this counts toward hangover
    int   hop_ms = 32;
    int   hangover_ms = 200;     // default; adaptive in the streamer
    int   onset_slow_hops = 2;   // consecutive slow hops to confirm onset
};

class VadStateMachine {
public:
    explicit VadStateMachine(VadConfig cfg = {}) : cfg_(cfg) {}

    // Feed one hop probability. `clause_incomplete` lets the caller lengthen the
    // hangover when the ASR partial ends mid-clause (adaptive hangover, §1.2.1).
    VadEvent push(float p, bool clause_incomplete = false) {
        switch (state_) {
            case VadState::Silence: {
                if (p >= cfg_.onset_fast) {
                    return enter_speech();
                }
                if (p >= cfg_.onset_slow) {
                    if (++slow_run_ >= cfg_.onset_slow_hops) {
                        return enter_speech();
                    }
                } else {
                    slow_run_ = 0;
                }
                return VadEvent::None;
            }
            case VadState::Speech: {
                const int hangover = clause_incomplete ? cfg_.hangover_ms * 2 : cfg_.hangover_ms;
                const int needed_hops = hangover / cfg_.hop_ms;
                if (p < cfg_.release) {
                    if (++silence_run_ >= needed_hops) {
                        state_ = VadState::Silence;
                        silence_run_ = 0;
                        slow_run_ = 0;
                        return VadEvent::SpeechEnd;
                    }
                } else {
                    silence_run_ = 0;  // speech resumed; reset hangover
                }
                return VadEvent::None;
            }
        }
        return VadEvent::None;
    }

    VadState state() const { return state_; }

private:
    VadEvent enter_speech() {
        state_ = VadState::Speech;
        slow_run_ = 0;
        silence_run_ = 0;
        return VadEvent::SpeechStart;
    }

    VadConfig cfg_;
    VadState state_ = VadState::Silence;
    int slow_run_ = 0;
    int silence_run_ = 0;
};

}  // namespace oracle
