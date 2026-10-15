/* C++ with the shared libstdc++ and libgcc_s (userspace/dynamic, ADR 0021):
 * exceptions thrown through frames and across threads (the unwinder finds the
 * frames' unwind tables through dl_iterate_phdr and PT_GNU_EH_FRAME),
 * std::thread, thread_local objects with destructors
 * (__cxa_thread_atexit_impl), static constructors, RTTI and iostreams. */
#include <atomic>
#include <cstdio>
#include <iostream>
#include <memory>
#include <mutex>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <typeinfo>
#include <vector>

static int failures;

static void check(const char *name, bool ok) {
    std::printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

struct Base {
    virtual ~Base() = default;
};
struct Derived : Base {
    int value = 17;
};

static int constructed;
struct Global {
    Global() { constructed = 1; }
};
static Global global;

static std::atomic<int> destroyed;
struct PerThread {
    int n = 0;
    ~PerThread() { destroyed++; }
};
static thread_local PerThread per_thread;

[[gnu::noinline]] static int deep(int n) {
    if (n == 0) throw std::runtime_error("from the bottom");
    std::string keep = "frame " + std::to_string(n);
    return deep(n - 1) + static_cast<int>(keep.size());
}

int main() {
    check("static constructors ran", constructed == 1);

    bool caught = false;
    try {
        deep(20);
    } catch (const std::runtime_error &e) {
        caught = std::string(e.what()) == "from the bottom";
    }
    check("an exception through 20 frames", caught);

    bool rethrown = false;
    try {
        try {
            throw 42;
        } catch (int) {
            throw;
        }
    } catch (int v) {
        rethrown = v == 42;
    }
    check("rethrow", rethrown);

    std::unique_ptr<Base> b(new Derived);
    check("dynamic_cast and typeid", dynamic_cast<Derived *>(b.get()) && dynamic_cast<Derived *>(b.get())->value == 17 && typeid(*b) == typeid(Derived));
    bool bad_cast = false;
    try {
        Base plain;
        (void)dynamic_cast<Derived &>(plain);
    } catch (const std::bad_cast &) {
        bad_cast = true;
    }
    check("std::bad_cast", bad_cast);

    std::mutex m;
    int sum = 0, thread_caught = 0;
    std::vector<std::thread> threads;
    for (int i = 0; i < 4; i++) {
        threads.emplace_back([&, i] {
            per_thread.n = i;
            try {
                deep(i + 1);
            } catch (const std::exception &) {
                std::lock_guard<std::mutex> g(m);
                thread_caught++;
            }
            std::lock_guard<std::mutex> g(m);
            sum += per_thread.n;
        });
    }
    for (auto &t : threads) t.join();
    check("std::thread with exceptions in each", thread_caught == 4 && sum == 0 + 1 + 2 + 3);
    check("thread_local destructors ran at each thread's end", destroyed == 4);

    std::ostringstream out;
    out << "x=" << 3.5 << ' ' << std::hex << 255;
    check("iostreams format", out.str() == "x=3.5 ff");
    std::cout << "dyncxx: " << (failures ? "failed" : "all passed") << std::endl;
    return failures ? 1 : 0;
}
