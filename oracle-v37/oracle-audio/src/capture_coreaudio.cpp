// CoreAudio capture backend (architecture §1.1), macOS only.
//
// Uses AUHAL -- an AudioUnit of subtype kAudioUnitSubType_HALOutput with input
// enabled -- which is the low-latency path on macOS. AudioQueue is the simpler
// API but buffers more, and this pipeline is built around barge-in, where the
// added latency is the whole cost.
//
// The unit is bound to the system default input device and pulls float frames
// at the device's native rate; they are downmixed to mono S16 and written into
// the SPSC ring. Like WASAPI, this does NOT resample -- the decimator stage
// downstream takes the native rate to 16k.
//
// This file compiles to nothing off macOS, so the Linux/Windows builds are
// unaffected. On macOS it provides the make_capture() factory.
//
// PERMISSION: macOS gates the microphone behind TCC. A bundled app needs
// NSMicrophoneUsageDescription in its Info.plist, and the user must approve the
// prompt; a plain CLI run inherits the terminal's grant. Without it the unit
// starts cleanly and delivers nothing but silence -- which is why start()
// reports the device it bound and the first-buffer watchdog below warns rather
// than failing quietly.

#include "oracle/capture.hpp"
#include "oracle/downmix.hpp"

#if defined(__APPLE__)

#include <AudioToolbox/AudioToolbox.h>
#include <AudioUnit/AudioUnit.h>
#include <CoreAudio/CoreAudio.h>

#include <atomic>
#include <cstdio>
#include <vector>

namespace oracle {

namespace {

// Resolve the system default input device.
AudioDeviceID default_input_device() {
    AudioDeviceID dev = kAudioObjectUnknown;
    UInt32 size = sizeof(dev);
    AudioObjectPropertyAddress addr{kAudioHardwarePropertyDefaultInputDevice,
                                    kAudioObjectPropertyScopeGlobal,
                                    kAudioObjectPropertyElementMain};
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &addr, 0, nullptr, &size, &dev) !=
        noErr) {
        return kAudioObjectUnknown;
    }
    return dev;
}

}  // namespace

class CoreAudioCapture final : public CaptureBackend {
public:
    explicit CoreAudioCapture(CaptureConfig cfg) : cfg_(cfg) {}
    ~CoreAudioCapture() override { stop(); }

    bool start(SpscRing& ring) override {
        ring_ = &ring;

        AudioComponentDescription desc{};
        desc.componentType = kAudioUnitType_Output;
        desc.componentSubType = kAudioUnitSubType_HALOutput;
        desc.componentManufacturer = kAudioUnitManufacturer_Apple;

        AudioComponent comp = AudioComponentFindNext(nullptr, &desc);
        if (comp == nullptr) {
            std::fprintf(stderr, "[coreaudio] no HAL output component\n");
            return false;
        }
        if (AudioComponentInstanceNew(comp, &unit_) != noErr || unit_ == nullptr) {
            std::fprintf(stderr, "[coreaudio] could not instantiate the audio unit\n");
            return false;
        }

        // AUHAL defaults to output-only. Enable IO on the input bus (element 1)
        // and disable it on the output bus (element 0), or the unit will try to
        // render to the speakers and never deliver input.
        UInt32 on = 1, off = 0;
        if (AudioUnitSetProperty(unit_, kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Input,
                                 kInputBus, &on, sizeof(on)) != noErr ||
            AudioUnitSetProperty(unit_, kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Output,
                                 kOutputBus, &off, sizeof(off)) != noErr) {
            std::fprintf(stderr, "[coreaudio] could not enable input IO\n");
            return fail();
        }

        AudioDeviceID dev = default_input_device();
        if (dev == kAudioObjectUnknown) {
            std::fprintf(stderr, "[coreaudio] no default input device\n");
            return fail();
        }
        if (AudioUnitSetProperty(unit_, kAudioOutputUnitProperty_CurrentDevice,
                                 kAudioUnitScope_Global, kOutputBus, &dev, sizeof(dev)) != noErr) {
            std::fprintf(stderr, "[coreaudio] could not bind the input device\n");
            return fail();
        }

        // Ask the hardware what it is actually producing rather than assuming:
        // built-in mics are commonly 1ch, aggregate and USB devices are not,
        // and the rate is whatever the device is clocked at.
        AudioStreamBasicDescription hw{};
        UInt32 sz = sizeof(hw);
        if (AudioUnitGetProperty(unit_, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input,
                                 kInputBus, &hw, &sz) != noErr) {
            std::fprintf(stderr, "[coreaudio] could not read the hardware format\n");
            return fail();
        }
        dev_channels_ = hw.mChannelsPerFrame ? hw.mChannelsPerFrame : 1;

        // Client-side format: float32, interleaved, same rate and channel count.
        // AUHAL converts into this for us; the downmix to mono S16 is ours.
        AudioStreamBasicDescription client{};
        client.mSampleRate = hw.mSampleRate;
        client.mFormatID = kAudioFormatLinearPCM;
        client.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
        client.mChannelsPerFrame = dev_channels_;
        client.mBitsPerChannel = 32;
        client.mFramesPerPacket = 1;
        client.mBytesPerFrame = sizeof(float) * dev_channels_;
        client.mBytesPerPacket = client.mBytesPerFrame;
        if (AudioUnitSetProperty(unit_, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output,
                                 kInputBus, &client, sizeof(client)) != noErr) {
            std::fprintf(stderr, "[coreaudio] device rejected the float32 client format\n");
            return fail();
        }

        AURenderCallbackStruct cb{};
        cb.inputProc = &CoreAudioCapture::render_cb;
        cb.inputProcRefCon = this;
        if (AudioUnitSetProperty(unit_, kAudioOutputUnitProperty_SetInputCallback,
                                 kAudioUnitScope_Global, kOutputBus, &cb, sizeof(cb)) != noErr) {
            std::fprintf(stderr, "[coreaudio] could not install the input callback\n");
            return fail();
        }

        if (AudioUnitInitialize(unit_) != noErr) {
            std::fprintf(stderr, "[coreaudio] AudioUnitInitialize failed\n");
            return fail();
        }
        if (AudioOutputUnitStart(unit_) != noErr) {
            std::fprintf(stderr, "[coreaudio] AudioOutputUnitStart failed\n");
            AudioUnitUninitialize(unit_);
            return fail();
        }

        running_.store(true);
        std::fprintf(stderr, "[coreaudio] capturing: %u ch @ %.0f Hz\n", dev_channels_,
                     hw.mSampleRate);
        return true;
    }

    void stop() override {
        if (unit_ == nullptr) return;
        if (running_.exchange(false)) {
            AudioOutputUnitStop(unit_);
            AudioUnitUninitialize(unit_);
        }
        AudioComponentInstanceDispose(unit_);
        unit_ = nullptr;

        if (!delivered_.load()) {
            // The unit ran and produced nothing. Almost always TCC: the process
            // was never granted the microphone, and macOS answers with silence
            // rather than an error.
            std::fprintf(stderr,
                         "[coreaudio] no audio was ever delivered. If this is unexpected, "
                         "check Privacy & Security -> Microphone for this binary.\n");
        }
    }

    const char* name() const override { return "coreaudio"; }

private:
    static constexpr AudioUnitElement kInputBus = 1;
    static constexpr AudioUnitElement kOutputBus = 0;

    bool fail() {
        if (unit_) {
            AudioComponentInstanceDispose(unit_);
            unit_ = nullptr;
        }
        return false;
    }

    // Called on CoreAudio's real-time thread. Nothing here allocates, locks, or
    // does I/O in the steady state: the scratch buffers are sized on the first
    // callback and reused, and SpscRing::write is wait-free.
    static OSStatus render_cb(void* ref, AudioUnitRenderActionFlags* flags,
                              const AudioTimeStamp* ts, UInt32 bus, UInt32 frames,
                              AudioBufferList* /*io*/) {
        auto* self = static_cast<CoreAudioCapture*>(ref);
        if (!self->running_.load()) return noErr;

        const size_t needed_floats = static_cast<size_t>(frames) * self->dev_channels_;
        if (self->scratch_.size() < needed_floats) self->scratch_.resize(needed_floats);
        if (self->mono_.size() < frames) self->mono_.resize(frames);

        AudioBufferList list{};
        list.mNumberBuffers = 1;
        list.mBuffers[0].mNumberChannels = self->dev_channels_;
        list.mBuffers[0].mDataByteSize = static_cast<UInt32>(needed_floats * sizeof(float));
        list.mBuffers[0].mData = self->scratch_.data();

        OSStatus st = AudioUnitRender(self->unit_, flags, ts, bus, frames, &list);
        if (st != noErr) return st;

        const size_t n =
            downmix_buffer_f32(self->scratch_.data(), frames, self->dev_channels_,
                               self->mono_.data());
        self->ring_->write(self->mono_.data(), n);
        self->delivered_.store(true);
        return noErr;
    }

    CaptureConfig cfg_;
    AudioUnit unit_ = nullptr;
    SpscRing* ring_ = nullptr;
    uint32_t dev_channels_ = 1;
    std::atomic<bool> running_{false};
    std::atomic<bool> delivered_{false};
    std::vector<float> scratch_;
    std::vector<int16_t> mono_;
};

std::unique_ptr<CaptureBackend> make_capture(CaptureConfig cfg) {
    return std::make_unique<CoreAudioCapture>(cfg);
}

}  // namespace oracle

#endif  // __APPLE__
