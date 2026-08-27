// ALSA capture backend (architecture §1.1). Compiled only when ORACLE_WITH_ALSA
// is set (needs libasound). Runs an SCHED_FIFO-eligible thread that reads PCM
// periods from the device and writes them into the SPSC ring. Overruns are
// handled by the ring's drop-oldest policy; xruns from ALSA are recovered.
#include "oracle/capture.hpp"

#include <atomic>
#include <cstdio>
#include <thread>
#include <vector>

#if defined(ORACLE_WITH_ALSA)
#include <alsa/asoundlib.h>
#include <pthread.h>
#include <sched.h>

namespace oracle {

class AlsaCapture final : public CaptureBackend {
public:
    explicit AlsaCapture(CaptureConfig cfg) : cfg_(cfg) {}
    ~AlsaCapture() override { stop(); }

    bool start(SpscRing& ring) override {
        if (snd_pcm_open(&pcm_, cfg_.device, SND_PCM_STREAM_CAPTURE, 0) < 0) {
            std::fprintf(stderr, "[alsa] cannot open device %s\n", cfg_.device);
            return false;
        }
        snd_pcm_hw_params_t* hw;
        snd_pcm_hw_params_alloca(&hw);
        snd_pcm_hw_params_any(pcm_, hw);
        snd_pcm_hw_params_set_access(pcm_, hw, SND_PCM_ACCESS_RW_INTERLEAVED);
        snd_pcm_hw_params_set_format(pcm_, hw, SND_PCM_FORMAT_S16_LE);
        snd_pcm_hw_params_set_channels(pcm_, hw, cfg_.channels);
        unsigned int rate = cfg_.sample_rate;
        snd_pcm_hw_params_set_rate_near(pcm_, hw, &rate, nullptr);
        snd_pcm_uframes_t period = cfg_.period_frames;
        snd_pcm_hw_params_set_period_size_near(pcm_, hw, &period, nullptr);
        if (snd_pcm_hw_params(pcm_, hw) < 0) {
            std::fprintf(stderr, "[alsa] hw params failed\n");
            snd_pcm_close(pcm_);
            pcm_ = nullptr;
            return false;
        }
        snd_pcm_prepare(pcm_);

        running_.store(true);
        thread_ = std::thread([this, &ring, period] { loop(ring, period); });
        return true;
    }

    void stop() override {
        running_.store(false);
        if (thread_.joinable()) thread_.join();
        if (pcm_) {
            snd_pcm_close(pcm_);
            pcm_ = nullptr;
        }
    }

    const char* name() const override { return "alsa"; }

private:
    void loop(SpscRing& ring, snd_pcm_uframes_t period) {
        // Best-effort RT scheduling for the capture thread.
        raise_priority();
        std::vector<int16_t> buf(period * cfg_.channels);
        while (running_.load()) {
            snd_pcm_sframes_t n = snd_pcm_readi(pcm_, buf.data(), period);
            if (n < 0) {
                // Recover from xrun/suspend.
                n = snd_pcm_recover(pcm_, static_cast<int>(n), 1);
                if (n < 0) {
                    std::fprintf(stderr, "[alsa] read error, unrecoverable\n");
                    break;
                }
                continue;
            }
            // Downmix to mono if needed (simple average) then write to the ring.
            if (cfg_.channels == 1) {
                ring.write(buf.data(), static_cast<size_t>(n));
            } else {
                mono_.resize(n);
                for (snd_pcm_sframes_t i = 0; i < n; ++i) {
                    int32_t acc = 0;
                    for (uint32_t c = 0; c < cfg_.channels; ++c)
                        acc += buf[i * cfg_.channels + c];
                    mono_[i] = static_cast<int16_t>(acc / static_cast<int32_t>(cfg_.channels));
                }
                ring.write(mono_.data(), static_cast<size_t>(n));
            }
        }
    }

    static void raise_priority() {
        sched_param sp{};
        sp.sched_priority = 80;  // requires CAP_SYS_NICE / rtprio limits
        pthread_setschedparam(pthread_self(), SCHED_FIFO, &sp);
    }

    CaptureConfig cfg_;
    snd_pcm_t* pcm_ = nullptr;
    std::atomic<bool> running_{false};
    std::thread thread_;
    std::vector<int16_t> mono_;
};

std::unique_ptr<CaptureBackend> make_capture(CaptureConfig cfg) {
    return std::make_unique<AlsaCapture>(cfg);
}

}  // namespace oracle

#elif !defined(_WIN32)  // no ALSA and not Windows → Null fallback (Linux w/o ALSA, etc.)

namespace oracle {
std::unique_ptr<CaptureBackend> make_capture(CaptureConfig cfg) {
    return std::make_unique<NullCapture>(cfg);
}
}  // namespace oracle

#endif
// On Windows without ALSA, make_capture() is provided by capture_wasapi.cpp.
