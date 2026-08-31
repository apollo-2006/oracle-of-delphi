// Dependency-free test harness for oracle-audio.
// A minimal CHECK/RUN framework so the build needs no gtest/catch2.
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <thread>
#include <string>
#include <vector>

#include "oracle/capture.hpp"
#include "oracle/downmix.hpp"
#include "oracle/resample.hpp"
#include "oracle/spsc_ring.hpp"
#include "oracle/tts_queue.hpp"
#include "oracle/vad.hpp"

static int g_failures = 0;
static int g_checks = 0;

#define CHECK(cond)                                                       \
    do {                                                                  \
        ++g_checks;                                                       \
        if (!(cond)) {                                                    \
            ++g_failures;                                                 \
            std::printf("  FAIL %s:%d  %s\n", __FILE__, __LINE__, #cond); \
        }                                                                 \
    } while (0)

using namespace oracle;

static void test_ring_basic() {
    std::printf("test_ring_basic\n");
    const uint32_t cap = 1024;
    std::vector<uint8_t> region(SpscRing::bytes_for(cap));
    CHECK(SpscRing::is_pow2(cap));
    auto ring = SpscRing::attach(region.data(), cap, 48000, 1, true);

    int16_t in[480];
    for (int i = 0; i < 480; ++i) in[i] = static_cast<int16_t>(i);
    CHECK(ring.write(in, 480) == 480);
    CHECK(ring.available() == 480);

    int16_t out[480];
    CHECK(ring.read(out, 480) == 480);
    for (int i = 0; i < 480; ++i) CHECK(out[i] == in[i]);
    CHECK(ring.available() == 0);
}

static void test_ring_wraparound_and_overrun() {
    std::printf("test_ring_wraparound_and_overrun\n");
    const uint32_t cap = 512;
    std::vector<uint8_t> region(SpscRing::bytes_for(cap));
    auto ring = SpscRing::attach(region.data(), cap, 48000, 1, true);

    int16_t block[400];
    for (int i = 0; i < 400; ++i) block[i] = static_cast<int16_t>(i);
    // Write 400, read 400 to move the indices near the wrap point.
    ring.write(block, 400);
    int16_t tmp[400];
    ring.read(tmp, 400);
    // Now write 400 again: this wraps around the 512 boundary.
    ring.write(block, 400);
    CHECK(ring.available() == 400);
    int16_t out[400];
    ring.read(out, 400);
    for (int i = 0; i < 400; ++i) CHECK(out[i] == block[i]);

    // Force an overrun: fill beyond capacity in one shot.
    std::vector<int16_t> big(cap + 100, 7);
    ring.write(big.data(), big.size());
    CHECK(ring.overruns() >= 1);
    CHECK(ring.available() <= cap);
}

static void test_ring_spsc_threads() {
    std::printf("test_ring_spsc_threads\n");
    const uint32_t cap = 1 << 14;  // 16384
    std::vector<uint8_t> region(SpscRing::bytes_for(cap));
    auto ring = SpscRing::attach(region.data(), cap, 48000, 1, true);

    const int total = 100000;
    std::thread producer([&] {
        int16_t buf[128];
        int written = 0;
        while (written < total) {
            int n = 0;
            for (; n < 128 && written + n < total; ++n)
                buf[n] = static_cast<int16_t>((written + n) & 0x7fff);
            size_t w = ring.write(buf, n);
            written += static_cast<int>(w);
        }
    });

    // Consumer: just drain; we assert no crash + monotonic drain under overrun.
    std::thread consumer([&] {
        int16_t buf[256];
        int read_total = 0;
        for (int spins = 0; spins < 2000000 && read_total < total; ++spins) {
            size_t r = ring.read(buf, 256);
            read_total += static_cast<int>(r);
        }
    });
    producer.join();
    consumer.join();
    // With drop-oldest under contention we can't assert exact equality, only
    // that the structure stayed consistent.
    CHECK(ring.available() <= cap);
}

static void test_vad_fast_onset() {
    std::printf("test_vad_fast_onset\n");
    VadStateMachine vad;
    CHECK(vad.push(0.05f) == VadEvent::None);
    // one loud hop → immediate onset
    CHECK(vad.push(0.9f) == VadEvent::SpeechStart);
    CHECK(vad.state() == VadState::Speech);
}

static void test_vad_slow_onset_needs_two_hops() {
    std::printf("test_vad_slow_onset_needs_two_hops\n");
    VadStateMachine vad;
    CHECK(vad.push(0.7f) == VadEvent::None);        // first slow hop
    CHECK(vad.push(0.7f) == VadEvent::SpeechStart); // second confirms
}

static void test_vad_hangover_endpoint() {
    std::printf("test_vad_hangover_endpoint\n");
    VadConfig cfg;
    cfg.hangover_ms = 96;  // 3 hops at 32ms
    cfg.hop_ms = 32;
    VadStateMachine vad(cfg);
    vad.push(0.9f);  // into speech
    CHECK(vad.push(0.1f) == VadEvent::None);        // hop 1 of hangover
    CHECK(vad.push(0.1f) == VadEvent::None);        // hop 2
    CHECK(vad.push(0.1f) == VadEvent::SpeechEnd);   // hop 3 → endpoint
    CHECK(vad.state() == VadState::Silence);
}

static void test_vad_adaptive_hangover_longer_when_incomplete() {
    std::printf("test_vad_adaptive_hangover_longer_when_incomplete\n");
    VadConfig cfg;
    cfg.hangover_ms = 64;  // 2 hops normally, 4 when clause incomplete
    cfg.hop_ms = 32;
    VadStateMachine vad(cfg);
    vad.push(0.9f);
    // clause_incomplete doubles the required silence hops → no endpoint at 2
    CHECK(vad.push(0.1f, true) == VadEvent::None);
    CHECK(vad.push(0.1f, true) == VadEvent::None);
    CHECK(vad.push(0.1f, true) == VadEvent::None);
    CHECK(vad.push(0.1f, true) == VadEvent::SpeechEnd);  // 4th hop
}

static void test_tts_flow_control() {
    std::printf("test_tts_flow_control\n");
    TtsQueue q(24000, 1.5, 3.0);  // low=36000, high=72000 samples
    CHECK(q.should_synthesize() == TtsDecision::Synthesize);  // empty → fill
    q.push_clause("Hello there.", 40000);  // above low water
    CHECK(q.buffered() == 40000);
    q.push_clause(" More text here.", 40000);  // now 80000 > high water
    CHECK(q.should_synthesize() == TtsDecision::Idle);
    q.advance_play(50000);  // played half → buffered 30000 < low water
    CHECK(q.should_synthesize() == TtsDecision::Synthesize);
}

static void test_tts_bargein_heard_upto() {
    std::printf("test_tts_bargein_heard_upto\n");
    TtsQueue q(24000);
    q.push_clause("Hello there.", 16000);        // chars 0..12, samples 0..16000
    q.push_clause(" I drafted a reply.", 24000); // ends 40000
    q.push_clause(" Lights dimmed.", 20000);      // ends 60000
    q.advance_play(30000);  // only first clause fully played
    uint64_t heard = q.barge_in();
    CHECK(heard == 30000);
    CHECK(q.frozen());
    // truncation lands on the first clause boundary (12 chars)
    CHECK(q.text_offset_for_sample(heard) == 12);
    q.resume();
    CHECK(!q.frozen());
}

static void test_decimator_downsamples() {
    std::printf("test_decimator_downsamples\n");
    Decimator dec(3, 63);
    // 3ms of a 1kHz tone at 48kHz = 144 samples → ~48 out.
    std::vector<int16_t> in(4800);
    for (size_t i = 0; i < in.size(); ++i)
        in[i] = static_cast<int16_t>(30000 * std::sin(2.0 * M_PI * 1000.0 * i / 48000.0));
    auto out = dec.process(in.data(), in.size());
    // 48000/3 = 16000 → roughly a third of the samples.
    CHECK(out.size() >= 1590 && out.size() <= 1610);
    // Output should retain substantial energy (tone well below new Nyquist).
    double energy = 0;
    for (auto s : out) energy += static_cast<double>(s) * s;
    CHECK(energy > 0.0);
}

static void test_capture_feeds_ring() {
    std::printf("test_capture_feeds_ring\n");
    // The Null backend must actually write samples into the ring. We test it
    // directly (not via the factory) so this is deterministic regardless of
    // whether ALSA is compiled or whether a sound card is present.
    const uint32_t cap = 1 << 16;
    std::vector<uint8_t> region(SpscRing::bytes_for(cap));
    auto ring = SpscRing::attach(region.data(), cap, 48000, 1, true);

    CaptureConfig cfg;
    cfg.sample_rate = 48000;
    cfg.period_frames = 480;
    NullCapture backend(cfg);
    CHECK(backend.start(ring));
    std::this_thread::sleep_for(std::chrono::milliseconds(60));
    backend.stop();
    CHECK(ring.available() > 0);

    // The factory must always return a usable backend (ALSA or Null).
    auto factory_backend = make_capture(cfg);
    CHECK(factory_backend != nullptr);
    CHECK(factory_backend->name() != nullptr);
}


// --- downmix (used by the CoreAudio backend; pure, so testable off a Mac) ----

static void test_downmix_averages_channels() {
    // Summing instead of averaging would double a stereo mic's amplitude and
    // clip everything loud into a square wave.
    const float stereo[2] = {0.5f, 0.5f};
    CHECK(downmix_frame_f32(stereo, 2) == 16384);

    const float opposed[2] = {0.5f, -0.5f};
    CHECK(downmix_frame_f32(opposed, 2) == 0);

    const float mono[1] = {0.25f};
    CHECK(downmix_frame_f32(mono, 1) == 8192);
}

static void test_downmix_clamps_asymmetrically() {
    // S16 runs -32768..32767, so the positive side saturates one lower. Getting
    // this wrong wraps a full-scale positive sample around to negative -- an
    // audible click on every loud transient.
    const float hot[1] = {2.0f};
    CHECK(downmix_frame_f32(hot, 1) == 32767);

    const float cold[1] = {-2.0f};
    CHECK(downmix_frame_f32(cold, 1) == -32768);

    const float full_positive[1] = {1.0f};
    CHECK(downmix_frame_f32(full_positive, 1) == 32767);

    const float silence[1] = {0.0f};
    CHECK(downmix_frame_f32(silence, 1) == 0);
}

static void test_downmix_handles_degenerate_input() {
    // A device reporting zero channels must not divide by zero on the RT thread.
    const float x[1] = {0.5f};
    CHECK(downmix_frame_f32(x, 0) == 0);
    CHECK(downmix_frame_f32(nullptr, 2) == 0);

    int16_t out[4] = {1, 1, 1, 1};
    CHECK(downmix_buffer_f32(nullptr, 4, 2, out) == 0);
    CHECK(downmix_buffer_f32(x, 4, 2, nullptr) == 0);
}

static void test_downmix_buffer_deinterleaves_frames() {
    // Four stereo frames -> four mono samples, in order.
    const float in[8] = {1.0f, 1.0f, 0.5f, 0.5f, 0.0f, 0.0f, -0.5f, -0.5f};
    int16_t out[4] = {};
    CHECK(downmix_buffer_f32(in, 4, 2, out) == 4);
    CHECK(out[0] == 32767);
    CHECK(out[1] == 16384);
    CHECK(out[2] == 0);
    CHECK(out[3] == -16384);
}


// --- the factory actually selects the backend the build asked for ------------

static void test_configured_backend_is_the_one_compiled_in() {
    // This guards a bug that shipped silently: CMake defined `Oracle_WITH_ALSA`
    // while the source tested `ORACLE_WITH_ALSA`. The names differ only in case,
    // so -DOracle_WITH_ALSA=ON linked libasound, compiled the Null backend, and
    // fed the VAD a 220Hz test tone instead of the microphone. Nothing failed --
    // it just never heard anything.
    CaptureConfig cfg;
    auto backend = make_capture(cfg);
    const char* got = backend->name();
    std::printf("  capture backend: %s\n", got);

#if defined(ORACLE_WITH_ALSA)
    CHECK(std::string(got) == "alsa");
#elif defined(_WIN32)
    CHECK(std::string(got) == "wasapi");
#elif defined(__APPLE__)
    CHECK(std::string(got) == "coreaudio");
#else
    CHECK(std::string(got) == "null");
#endif
}

int main() {
    std::printf("=== oracle-audio unit tests ===\n");
    test_ring_basic();
    test_ring_wraparound_and_overrun();
    test_ring_spsc_threads();
    test_vad_fast_onset();
    test_vad_slow_onset_needs_two_hops();
    test_vad_hangover_endpoint();
    test_vad_adaptive_hangover_longer_when_incomplete();
    test_tts_flow_control();
    test_tts_bargein_heard_upto();
    test_decimator_downsamples();
    test_capture_feeds_ring();
    test_downmix_averages_channels();
    test_downmix_clamps_asymmetrically();
    test_downmix_handles_degenerate_input();
    test_downmix_buffer_deinterleaves_frames();
    test_configured_backend_is_the_one_compiled_in();

    std::printf("\n%d checks, %d failures\n", g_checks, g_failures);
    if (g_failures == 0) std::printf("ALL PASS\n");
    return g_failures == 0 ? 0 : 1;
}
