// WASAPI capture backend (architecture §1.1), Windows only.
//
// Uses the Windows Core Audio APIs (IMMDeviceEnumerator / IAudioClient /
// IAudioCaptureClient) to pull PCM from a capture endpoint in shared, event-
// driven mode and write it into the SPSC ring. It selects the device by the
// friendly name in CaptureConfig.device (e.g. "Microphone (Razer Seiren V3
// Mini)"), falling back to the default capture endpoint. Float samples from the
// mixer are converted to S16 and downmixed to mono for the VAD/ASR stage.
//
// This file compiles to nothing on non-Windows targets, so the Linux/CI build
// is unaffected. On Windows it provides the make_capture() factory. lowk essential.

#include "oracle/capture.hpp"

#if defined(_WIN32)

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <functiondiscoverykeys_devpkey.h>

#include <atomic>
#include <cstdio>
#include <string>
#include <thread>
#include <vector>

namespace oracle {

namespace {

// RAII for CoInitialize on the capture thread.
struct ComInit {
    HRESULT hr;
    ComInit() { hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED); }
    ~ComInit() { if (SUCCEEDED(hr)) CoUninitialize(); }
};

std::string wide_to_utf8(const wchar_t* w) {
    if (!w) return {};
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, nullptr, 0, nullptr, nullptr);
    std::string s(n > 0 ? n - 1 : 0, '\0');
    if (n > 0) WideCharToMultiByte(CP_UTF8, 0, w, -1, s.data(), n, nullptr, nullptr);
    return s;
}

// Find a capture device whose friendly name contains `want`; returns nullptr to
// mean "use default".
IMMDevice* find_device_by_name(IMMDeviceEnumerator* en, const std::string& want) {
    if (want.empty() || want == "default") return nullptr;
    IMMDeviceCollection* coll = nullptr;
    if (FAILED(en->EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE, &coll))) return nullptr;
    UINT count = 0;
    coll->GetCount(&count);
    IMMDevice* found = nullptr;
    for (UINT i = 0; i < count && !found; ++i) {
        IMMDevice* dev = nullptr;
        if (FAILED(coll->Item(i, &dev))) continue;
        IPropertyStore* props = nullptr;
        if (SUCCEEDED(dev->OpenPropertyStore(STGM_READ, &props))) {
            PROPVARIANT name;
            PropVariantInit(&name);
            if (SUCCEEDED(props->GetValue(PKEY_Device_FriendlyName, &name)) && name.pwszVal) {
                std::string friendly = wide_to_utf8(name.pwszVal);
                if (friendly.find(want) != std::string::npos) {
                    found = dev;   // keep this ref
                    dev = nullptr; // don't release below
                }
            }
            PropVariantClear(&name);
            props->Release();
        }
        if (dev) dev->Release();
    }
    coll->Release();
    return found;
}

}  // namespace

class WasapiCapture final : public CaptureBackend {
public:
    explicit WasapiCapture(CaptureConfig cfg) : cfg_(cfg) {}
    ~WasapiCapture() override { stop(); }

    bool start(SpscRing& ring) override {
        running_.store(true);
        thread_ = std::thread([this, &ring] { run(ring); });
        return true;
    }

    void stop() override {
        running_.store(false);
        if (thread_.joinable()) thread_.join();
    }

    const char* name() const override { return "wasapi"; }

private:
    void run(SpscRing& ring) {
        ComInit com;
        if (FAILED(com.hr)) { std::fprintf(stderr, "[wasapi] CoInitialize failed\n"); return; }

        IMMDeviceEnumerator* en = nullptr;
        if (FAILED(CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                                    __uuidof(IMMDeviceEnumerator), (void**)&en))) {
            std::fprintf(stderr, "[wasapi] no device enumerator\n");
            return;
        }

        IMMDevice* dev = find_device_by_name(en, cfg_.device ? cfg_.device : "");
        if (!dev) {
            if (FAILED(en->GetDefaultAudioEndpoint(eCapture, eConsole, &dev))) {
                std::fprintf(stderr, "[wasapi] no default capture endpoint\n");
                en->Release();
                return;
            }
        }

        IAudioClient* client = nullptr;
        if (FAILED(dev->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, (void**)&client))) {
            std::fprintf(stderr, "[wasapi] Activate failed\n");
            dev->Release(); en->Release(); return;
        }

        WAVEFORMATEX* mix = nullptr;
        client->GetMixFormat(&mix);
        const uint32_t dev_rate = mix->nSamplesPerSec;
        const uint32_t dev_ch = mix->nChannels;
        const bool is_float = (mix->wFormatTag == WAVE_FORMAT_IEEE_FLOAT) ||
            (mix->wFormatTag == WAVE_FORMAT_EXTENSIBLE);

        HANDLE evt = CreateEvent(nullptr, FALSE, FALSE, nullptr);
        REFERENCE_TIME buf_dur = 10 * 10000;  // 10ms in 100ns units
        HRESULT hr = client->Initialize(AUDCLNT_SHAREMODE_SHARED,
                                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                                        buf_dur, 0, mix, nullptr);
        if (FAILED(hr)) {
            std::fprintf(stderr, "[wasapi] Initialize failed (0x%lx)\n", hr);
            CoTaskMemFree(mix); client->Release(); dev->Release(); en->Release(); return;
        }
        client->SetEventHandle(evt);

        IAudioCaptureClient* capture = nullptr;
        client->GetService(__uuidof(IAudioCaptureClient), (void**)&capture);
        client->Start();
        std::fprintf(stderr, "[wasapi] capturing %u Hz x%u %s\n", dev_rate, dev_ch,
                     is_float ? "float" : "pcm");

        std::vector<int16_t> mono;
        while (running_.load()) {
            if (WaitForSingleObject(evt, 200) != WAIT_OBJECT_0) continue;
            UINT32 packet = 0;
            capture->GetNextPacketSize(&packet);
            while (packet != 0 && running_.load()) {
                BYTE* data = nullptr;
                UINT32 frames = 0;
                DWORD flags = 0;
                if (FAILED(capture->GetBuffer(&data, &frames, &flags, nullptr, nullptr))) break;
                mono.resize(frames);
                const bool silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT);
                for (UINT32 i = 0; i < frames; ++i) {
                    float acc = 0.0f;
                    if (!silent && data) {
                        if (is_float) {
                            const float* f = reinterpret_cast<const float*>(data);
                            for (uint32_t c = 0; c < dev_ch; ++c) acc += f[i * dev_ch + c];
                        } else {
                            const int16_t* s = reinterpret_cast<const int16_t*>(data);
                            for (uint32_t c = 0; c < dev_ch; ++c)
                                acc += s[i * dev_ch + c] / 32768.0f;
                        }
                        acc /= static_cast<float>(dev_ch);
                    }
                    int v = static_cast<int>(acc * 32768.0f);
                    if (v > 32767) v = 32767; if (v < -32768) v = -32768;
                    mono[i] = static_cast<int16_t>(v);
                }
                ring.write(mono.data(), mono.size());
                capture->ReleaseBuffer(frames);
                capture->GetNextPacketSize(&packet);
            }
        }

        client->Stop();
        capture->Release();
        CoTaskMemFree(mix);
        client->Release();
        dev->Release();
        en->Release();
        CloseHandle(evt);
    }

    CaptureConfig cfg_;
    std::atomic<bool> running_{false};
    std::thread thread_;
};

// On Windows, WASAPI is the capture factory. (Note: this device delivers the
// mixer's native rate; the decimator/feature stage resamples to 16k as needed.)
std::unique_ptr<CaptureBackend> make_capture(CaptureConfig cfg) {
    return std::make_unique<WasapiCapture>(cfg);
}

}  // namespace oracle

#endif  // _WIN32
