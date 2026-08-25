// 48k → 16k decimation (architecture §1.1).
//
// Production uses a 64-tap polyphase FIR. Here we implement a straightforward
// windowed-sinc FIR decimator by an integer factor (3 for 48k→16k). It is real
// and correct (just not SIMD-optimized), so the feature stage that follows it
// gets genuinely band-limited 16k audio.
#pragma once
#include <cmath>
#include <cstdint>
#include <vector>

namespace oracle {

class Decimator {
public:
    // factor = in_rate / out_rate (e.g. 3 for 48000→16000). num_taps controls
    // the anti-alias filter sharpness.
    explicit Decimator(int factor = 3, int num_taps = 63)
        : factor_(factor), taps_(design(factor, num_taps)),
          history_(taps_.size(), 0.0f) {}

    // Push interleaved-mono samples; returns decimated output samples.
    std::vector<int16_t> process(const int16_t* in, size_t n) {
        std::vector<int16_t> out;
        out.reserve(n / factor_ + 1);
        for (size_t i = 0; i < n; ++i) {
            // shift history
            history_.erase(history_.begin());
            history_.push_back(static_cast<float>(in[i]) / 32768.0f);
            if (++phase_ >= factor_) {
                phase_ = 0;
                float acc = 0.0f;
                for (size_t t = 0; t < taps_.size(); ++t) {
                    acc += taps_[t] * history_[history_.size() - 1 - t];
                }
                int v = static_cast<int>(std::lround(acc * 32768.0f));
                if (v > 32767) v = 32767;
                if (v < -32768) v = -32768;
                out.push_back(static_cast<int16_t>(v));
            }
        }
        return out;
    }

private:
    // Windowed-sinc low-pass at the Nyquist of the output rate.
    static std::vector<float> design(int factor, int num_taps) {
        std::vector<float> h(num_taps);
        const double fc = 0.5 / factor;  // normalized cutoff (relative to in-rate)
        const int mid = num_taps / 2;
        double sum = 0.0;
        for (int i = 0; i < num_taps; ++i) {
            int k = i - mid;
            double sinc = (k == 0) ? 2.0 * fc
                                   : std::sin(2.0 * M_PI * fc * k) / (M_PI * k);
            // Hamming window
            double w = 0.54 - 0.46 * std::cos(2.0 * M_PI * i / (num_taps - 1));
            h[i] = static_cast<float>(sinc * w);
            sum += h[i];
        }
        for (auto& x : h) x /= static_cast<float>(sum);  // unity DC gain
        return h;
    }

    int factor_;
    int phase_ = 0;
    std::vector<float> taps_;
    std::vector<float> history_;
};

}  // namespace oracle
