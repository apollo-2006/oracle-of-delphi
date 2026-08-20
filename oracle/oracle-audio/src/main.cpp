// oracle-audio: the real-time audio engine (architecture §1).
//
// Production: RT-scheduled capture (ALSA/WASAPI) → SPSC ring → decimate → VAD →
// streaming ASR (HIP) → transcripts to core; TTS clauses from core → vocoder →
// output ring → device, with a <75ms barge-in interrupt chain.
//
// This reference `main` runs a synthetic pipeline end-to-end with no audio
// hardware: it generates a "speech-then-silence" probability track, drives the
// VAD, simulates active TTS, and demonstrates the barge-in interrupt producing
// a sample-accurate heard-upto offset — the crux of §1.2.3.
#include <cstdint>
#include <cstdio>
#include <vector>

#include "oracle/capture.hpp"
#include "oracle/spsc_ring.hpp"
#include "oracle/tts_queue.hpp"
#include "oracle/vad.hpp"

using namespace oracle;

int main() {
    std::printf("[oracle-audio] synthetic pipeline self-check\n");

    // --- Capture ring, fed by the real capture backend (ALSA if built,
    //     else the Null tone generator) ---
    const uint32_t cap = 1 << 16;
    std::vector<uint8_t> region(SpscRing::bytes_for(cap));
    auto ring = SpscRing::attach(region.data(), cap, 48000, 1, true);
    CaptureConfig cap_cfg;
    auto backend = make_capture(cap_cfg);
    std::printf("[capture] backend=%s\n", backend->name());
    backend->start(ring);
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    backend->stop();
    std::printf("[capture] %zu samples captured, %llu overruns\n",
                ring.available(), (unsigned long long)ring.overruns());

    // --- Assistant is mid-sentence: TTS queue has audio playing ---
    TtsQueue tts(24000);
    tts.push_clause("You have one unread email from your advisor.", 48000);
    tts.push_clause(" I drafted a reply offering 2 PM.", 36000);
    tts.push_clause(" Dimming the lights now.", 24000);
    tts.advance_play(60000);  // 60000 samples have reached the speaker
    std::printf("[tts] buffered=%llu played=%llu\n",
                (unsigned long long)tts.buffered(),
                (unsigned long long)tts.played_samples());

    // --- User starts speaking over the assistant: barge-in ---
    VadStateMachine vad;
    // silence, silence, then a loud onset hop
    vad.push(0.02f);
    VadEvent ev = vad.push(0.95f);
    if (ev == VadEvent::SpeechStart) {
        uint64_t heard = tts.barge_in();
        size_t chars = tts.text_offset_for_sample(heard);
        std::printf("[barge-in] onset detected; heard_upto=%llu samples → %zu chars spoken\n",
                    (unsigned long long)heard, chars);
        std::printf("[barge-in] TTS frozen=%d — core truncates assistant turn to what was heard\n",
                    (int)tts.frozen());
    }

    std::printf("[oracle-audio] self-check complete\n");
    return 0;
}
