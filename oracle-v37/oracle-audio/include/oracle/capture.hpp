// Capture backend interface (architecture §1.1).
//
// One interface, multiple OS backends: ALSA (Linux), WASAPI (Windows). A
// backend runs a real-time thread that pulls PCM from the device and writes it
// into the SPSC ring with zero copies beyond the driver boundary. The Null
// backend feeds silence/a test tone so the pipeline runs with no hardware.
#pragma once
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <functional>
#include <memory>
#include <thread>
#include <vector>

#include "oracle/spsc_ring.hpp"

namespace oracle {

struct CaptureConfig {
    const char* device = "default";
    uint32_t sample_rate = 48000;
    uint32_t channels = 1;
    uint32_t period_frames = 480;  // 10ms @ 48k
};

// Abstract capture source. `start` launches the capture thread which writes
// into `ring`; `stop` joins it.
class CaptureBackend {
public:
    virtual ~CaptureBackend() = default;
    virtual bool start(SpscRing& ring) = 0;
    virtual void stop() = 0;
    virtual const char* name() const = 0;
};

// A hardware-free backend: synthesizes a low-level test tone so the VAD/ASR
// stages have real signal to process without a sound card. Used in CI and the
// self-check.
class NullCapture final : public CaptureBackend {
public:
    explicit NullCapture(CaptureConfig cfg) : cfg_(cfg) {}
    ~NullCapture() override { stop(); }

    bool start(SpscRing& ring) override {
        running_.store(true);
        thread_ = std::thread([this, &ring] {
            std::vector<int16_t> period(cfg_.period_frames, 0);
            uint64_t phase = 0;
            while (running_.load()) {
                // 220Hz sine at low amplitude.
                for (uint32_t i = 0; i < cfg_.period_frames; ++i) {
                    double t = static_cast<double>(phase++) / cfg_.sample_rate;
                    period[i] = static_cast<int16_t>(1200.0 * std::sin(2.0 * M_PI * 220.0 * t));
                }
                ring.write(period.data(), period.size());
                // Pace roughly at real time (period duration).
                std::this_thread::sleep_for(std::chrono::microseconds(
                    static_cast<int64_t>(1e6 * cfg_.period_frames / cfg_.sample_rate)));
            }
        });
        return true;
    }

    void stop() override {
        running_.store(false);
        if (thread_.joinable()) thread_.join();
    }

    const char* name() const override { return "null"; }

private:
    CaptureConfig cfg_;
    std::atomic<bool> running_{false};
    std::thread thread_;
};

// Factory: returns the best real backend if compiled in, else Null.
std::unique_ptr<CaptureBackend> make_capture(CaptureConfig cfg);

}  // namespace oracle
