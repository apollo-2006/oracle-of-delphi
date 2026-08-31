// Float32 -> S16 mono conversion, shared by the capture backends.
//
// Every real capture device hands back float samples at the device's channel
// count, while the VAD/ASR stage downstream wants mono S16. That conversion is
// pure arithmetic with two easy mistakes in it -- averaging vs summing, and
// asymmetric clamping -- so it lives here, out of the platform files, where it
// can be tested on any host rather than only on the OS whose backend uses it.
#pragma once
#include <cstdint>
#include <cstddef>

namespace oracle {

// Convert one interleaved float frame to a single S16 sample.
//
// Channels are AVERAGED, not summed: summing a stereo mic would double the
// amplitude and clip everything loud into a square wave.
//
// The clamp is asymmetric because two's complement is: S16 runs -32768..32767,
// so the positive side saturates one below the negative. Scaling by 32768 and
// clamping the top to 32767 keeps unity gain without wrapping a full-scale
// positive sample around to negative.
inline int16_t downmix_frame_f32(const float* frame, uint32_t channels) {
    if (frame == nullptr || channels == 0) return 0;
    float acc = 0.0f;
    for (uint32_t c = 0; c < channels; ++c) acc += frame[c];
    acc /= static_cast<float>(channels);

    float scaled = acc * 32768.0f;
    if (scaled > 32767.0f) scaled = 32767.0f;
    if (scaled < -32768.0f) scaled = -32768.0f;
    return static_cast<int16_t>(scaled);
}

// Convert `frames` interleaved float frames into `out` (which must hold at
// least `frames` samples). Returns the number of samples written.
inline size_t downmix_buffer_f32(const float* in, size_t frames, uint32_t channels,
                                 int16_t* out) {
    if (in == nullptr || out == nullptr || channels == 0) return 0;
    for (size_t i = 0; i < frames; ++i) {
        out[i] = downmix_frame_f32(in + (i * channels), channels);
    }
    return frames;
}

}  // namespace oracle
