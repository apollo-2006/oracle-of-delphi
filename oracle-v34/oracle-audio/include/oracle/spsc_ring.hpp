// Lock-free SPSC ring buffer (architecture §1.1).
//
// Single-producer / single-consumer audio ring. Head/tail are monotonically
// increasing counters (never wrapped); the index is `counter & mask`, which
// removes the classic full/empty ambiguity and gives free overrun accounting.
// The producer (audio callback thread) never locks, never allocates, never
// syscalls. Overrun policy is drop-oldest: for live speech, fresh audio always
// beats stale audio.
//
// The buffer lives in a caller-provided region so it can be placed in
// shared memory (shm_open / CreateFileMapping) for cross-process zero-copy.
#pragma once
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstring>

namespace oracle {

// Header placed at the front of the (optionally shared) region.
struct alignas(64) RingHeader {
    std::atomic<uint64_t> head;   // producer: total samples written
    char _pad0[64 - sizeof(std::atomic<uint64_t>)];
    std::atomic<uint64_t> tail;   // consumer: total samples read
    char _pad1[64 - sizeof(std::atomic<uint64_t>)];
    std::atomic<uint64_t> overruns;
    uint32_t capacity;            // power of two, in samples
    uint32_t sample_rate;
    uint32_t channels;
};

// A view over a header + sample storage. Does not own memory.
class SpscRing {
public:
    // `region` must be at least sizeof(RingHeader) + capacity*sizeof(int16_t).
    // `capacity` must be a power of two.
    static SpscRing attach(void* region, uint32_t capacity, uint32_t sample_rate,
                           uint32_t channels, bool init) {
        auto* h = reinterpret_cast<RingHeader*>(region);
        auto* buf = reinterpret_cast<int16_t*>(
            reinterpret_cast<uint8_t*>(region) + sizeof(RingHeader));
        if (init) {
            h->head.store(0, std::memory_order_relaxed);
            h->tail.store(0, std::memory_order_relaxed);
            h->overruns.store(0, std::memory_order_relaxed);
            h->capacity = capacity;
            h->sample_rate = sample_rate;
            h->channels = channels;
        }
        return SpscRing(h, buf);
    }

    static constexpr size_t bytes_for(uint32_t capacity) {
        return sizeof(RingHeader) + static_cast<size_t>(capacity) * sizeof(int16_t);
    }

    static bool is_pow2(uint32_t x) { return x && ((x & (x - 1)) == 0); }

    // Producer side. Returns number of samples written (== n unless something
    // is very wrong). Drop-oldest on overrun.
    size_t write(const int16_t* src, size_t n) noexcept {
        const uint32_t cap = h_->capacity;
        const uint64_t head = h_->head.load(std::memory_order_relaxed);
        const uint64_t tail = h_->tail.load(std::memory_order_acquire);
        const uint64_t used = head - tail;
        if (cap - used < n) {
            // Advance tail to drop the oldest samples we're about to overwrite.
            const uint64_t drop = n - (cap - used);
            h_->tail.store(tail + drop, std::memory_order_release);
            h_->overruns.fetch_add(1, std::memory_order_relaxed);
        }
        const size_t idx = static_cast<size_t>(head & (cap - 1));
        const size_t first = (n < cap - idx) ? n : (cap - idx);
        std::memcpy(buf_ + idx, src, first * sizeof(int16_t));
        if (n > first) {
            std::memcpy(buf_, src + first, (n - first) * sizeof(int16_t));
        }
        h_->head.store(head + n, std::memory_order_release);  // publish
        return n;
    }

    // Consumer side. Copies up to `max_n` samples into `dst`; returns count.
    size_t read(int16_t* dst, size_t max_n) noexcept {
        const uint32_t cap = h_->capacity;
        const uint64_t tail = h_->tail.load(std::memory_order_relaxed);
        const uint64_t head = h_->head.load(std::memory_order_acquire);
        const uint64_t avail = head - tail;
        const size_t n = (avail < max_n) ? static_cast<size_t>(avail) : max_n;
        const size_t idx = static_cast<size_t>(tail & (cap - 1));
        const size_t first = (n < cap - idx) ? n : (cap - idx);
        std::memcpy(dst, buf_ + idx, first * sizeof(int16_t));
        if (n > first) {
            std::memcpy(dst + first, buf_, (n - first) * sizeof(int16_t));
        }
        h_->tail.store(tail + n, std::memory_order_release);
        return n;
    }

    uint64_t overruns() const { return h_->overruns.load(std::memory_order_relaxed); }
    size_t available() const {
        return static_cast<size_t>(h_->head.load(std::memory_order_acquire) -
                                   h_->tail.load(std::memory_order_acquire));
    }
    uint32_t capacity() const { return h_->capacity; }

private:
    SpscRing(RingHeader* h, int16_t* buf) : h_(h), buf_(buf) {}
    RingHeader* h_;
    int16_t* buf_;
};

}  // namespace oracle
