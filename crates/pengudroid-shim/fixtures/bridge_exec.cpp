// Standalone ABI/interposition probe using the pinned Digitalis headers.
// Build modes and target-only artifacts are managed by tests/bridge_exec.py.
#include "berberis/base/exec_region.h"
#include "berberis/runtime_primitives/exec_region_anonymous.h"
#include "berberis/runtime_primitives/exec_region_elf_backed.h"

#include <dirent.h>
#include <dlfcn.h>
#include <sys/mman.h>
#include <unistd.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <new>
#include <type_traits>
#include <utility>

using berberis::ExecRegion;
using berberis::ExecRegionAnonymousFactory;
using berberis::ExecRegionElfBackedFactory;
constexpr size_t kCapacity = 512 * 1024;
static_assert(sizeof(ExecRegion) == 24 && alignof(ExecRegion) == 8);
static_assert(std::is_standard_layout_v<ExecRegion>);
static_assert(!std::is_trivially_move_constructible_v<ExecRegion>);
static_assert(!std::is_copy_constructible_v<ExecRegion>);
static_assert(ExecRegionElfBackedFactory::kExecRegionSize == kCapacity);

#define REQUIRE(condition) do { if (!(condition)) { \
    std::fprintf(stderr, "%s:%d: %s\n", __FILE__, __LINE__, #condition); \
    std::exit(2); \
} } while (false)

#if defined(BRIDGE_EXEC_ABI) || defined(BRIDGE_EXEC_FAKE_BACKEND)
alignas(64) static uint8_t fake_exec[kCapacity];
alignas(64) static uint8_t fake_write[kCapacity];
static size_t delegate_size;
namespace berberis {
ExecRegion ExecRegionAnonymousFactory::Create(size_t size) {
    REQUIRE(size <= kCapacity);
    delegate_size = size;
    // Real, live C++ storage and the actual nontrivial class: not a fabricated
    // C aggregate-return function pretending to have the C++ factory ABI.
    return ExecRegion(fake_exec, fake_write, size);
}
}  // namespace berberis
extern "C" size_t bridge_exec_delegate_size() { return delegate_size; }
extern "C" const uint8_t* bridge_exec_fake_exec() { return fake_exec; }
extern "C" const uint8_t* bridge_exec_fake_write() { return fake_write; }
#endif

#if defined(BRIDGE_EXEC_FAKE_BACKEND) || defined(BRIDGE_EXEC_REAL_BACKEND)
namespace berberis {
__attribute__((noinline)) ExecRegion ExecRegionElfBackedFactory::Create(size_t) {
    std::fputs("probe: original ELF-backed factory reached\n", stderr);
    _exit(90);
}
}  // namespace berberis
extern "C" void* bridge_exec_make(void* output, size_t requested) {
    // This is an internal, preemptible C++ call from the fake translator DSO.
    // C++17 elision makes the caller's guarded storage the actual sret slot.
    return ::new (output) ExecRegion(ExecRegionElfBackedFactory::Create(requested));
}
#if defined(BRIDGE_EXEC_REAL_BACKEND)
extern "C" void bridge_exec_write(ExecRegion* region, const void* code, size_t size) {
    region->Write(region->begin(), code, size);
}
extern "C" void bridge_exec_detach(ExecRegion* region) { region->Detach(); }
extern "C" void bridge_exec_free(ExecRegion* region) { region->Free(); }
#endif
#endif

#if defined(BRIDGE_EXEC_ABI) || defined(BRIDGE_EXEC_DRIVER)
struct GuardedResult {
    uint8_t* mapping;
    uint8_t* output;
    size_t page;
    bool leading;
    explicit GuardedResult(bool guard_before) : leading(guard_before) {
        page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
        mapping = static_cast<uint8_t*>(mmap(nullptr, page * 3, PROT_NONE,
                                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        REQUIRE(mapping != MAP_FAILED);
        REQUIRE(mprotect(mapping + page, page, PROT_READ | PROT_WRITE) == 0);
        std::memset(mapping + page, 0xa5, page);
        output = mapping + (leading ? page : page * 2 - sizeof(ExecRegion));
    }
    ExecRegion* region() { return std::launder(reinterpret_cast<ExecRegion*>(output)); }
    void check() const {
        auto* canary = leading ? output + sizeof(ExecRegion) : output - 32;
        for (size_t i = 0; i < 32; ++i) REQUIRE(canary[i] == 0xa5);
    }
    ~GuardedResult() { check(); REQUIRE(munmap(mapping, page * 3) == 0); }
};

static void check_fake(GuardedResult& storage, const uint8_t* exec,
                       const uint8_t* write, size_t expected_size) {
    auto* region = storage.region();
    REQUIRE(region->begin() == exec && region->size() == expected_size);
    REQUIRE(region->end() == exec + expected_size);
    // Only the probe inspects object representation; the Rust hook treats the
    // hidden return slot as opaque. All three pinned fields must survive.
    uint8_t* observed_write;
    std::memcpy(&observed_write, storage.output + sizeof(void*), sizeof(observed_write));
    REQUIRE(observed_write == write);
    ExecRegion moved(std::move(*region));
    REQUIRE(region->begin() == nullptr && region->size() == 0);
    *region = std::move(moved);
    REQUIRE(moved.begin() == nullptr && moved.size() == 0);
    REQUIRE(region->begin() == exec && region->size() == expected_size);
    storage.check();
    region->~ExecRegion();
}
#endif

#if defined(BRIDGE_EXEC_ABI)
// Verify the machine-level lowering *before* involving Rust. The assembly
// adapter has the explicit C ABI that the hook uses, then tail-jumps to a real
// compiler-generated nontrivial C++ return. RAX must be the hidden result ptr.
extern "C" void* bridge_exec_lowered(void*, size_t);
__asm__(
    ".text\n.globl bridge_exec_lowered\n.type bridge_exec_lowered,@function\n"
    "bridge_exec_lowered:\n"
    "jmp _ZN8berberis26ExecRegionAnonymousFactory6CreateEm@PLT\n"
    ".size bridge_exec_lowered,.-bridge_exec_lowered\n");
int main() {
    const size_t requests[] = {0, 1, 17, kCapacity};
    for (bool leading : {false, true}) {
        for (size_t requested : requests) {
            GuardedResult result(leading);
            REQUIRE(bridge_exec_lowered(result.output, requested) == result.output);
            REQUIRE(delegate_size == requested);
            check_fake(result, fake_exec, fake_write, requested);
        }
    }
    std::puts("compiler C++ ABI: hidden output in RDI, size in RSI, output returned in RAX; exact 24-byte guard pages passed");
}
#endif

#if defined(BRIDGE_EXEC_DRIVER)
template <typename T> static T symbol(void* handle, const char* name) {
    auto address = dlsym(handle, name);
    if (!address) std::fprintf(stderr, "missing probe symbol %s: %s\n", name, dlerror());
    REQUIRE(address != nullptr);
    return reinterpret_cast<T>(address);
}

struct Mapping {
    uintptr_t start = 0, end = 0;
    char perms[5] = {}, device[32] = {};
    unsigned long offset = 0, inode = 0;
};
static Mapping find_mapping(uintptr_t address, const Mapping* same_file = nullptr) {
    FILE* maps = std::fopen("/proc/self/maps", "r");
    REQUIRE(maps != nullptr);
    char line[1024];
    Mapping found;
    while (std::fgets(line, sizeof(line), maps)) {
        Mapping entry;
        if (std::sscanf(line, "%lx-%lx %4s %lx %31s %lu", &entry.start, &entry.end,
                        entry.perms, &entry.offset, entry.device, &entry.inode) != 6) continue;
        const bool match = same_file ? (entry.inode == same_file->inode &&
            std::strcmp(entry.device, same_file->device) == 0 && entry.perms[1] == 'w')
            : (entry.start <= address && address < entry.end);
        if (match) { found = entry; break; }
    }
    REQUIRE(std::fclose(maps) == 0);
    return found;
}
static size_t fd_count() {
    DIR* directory = opendir("/proc/self/fd");
    REQUIRE(directory != nullptr);
    size_t count = 0;
    while (readdir(directory)) ++count;
    REQUIRE(closedir(directory) == 0);
    return count;
}

static void check_real(void* backend, GuardedResult& storage) {
    auto* region = storage.region();
    REQUIRE(region->begin() != nullptr && region->size() == kCapacity);
    const auto exec = reinterpret_cast<uintptr_t>(region->begin());
    REQUIRE(exec + region->size() <= uintptr_t{0x80000000});
    const Mapping rx = find_mapping(exec);
    const Mapping rw = find_mapping(0, &rx);
    REQUIRE(rx.start == exec && rx.end - rx.start == kCapacity && rx.inode != 0);
    REQUIRE(std::strcmp(rx.perms, "r-xs") == 0);
    REQUIRE(rw.start != 0 && rw.start != rx.start && rw.end - rw.start == kCapacity);
    REQUIRE(std::strcmp(rw.perms, "rw-s") == 0 && rw.offset == rx.offset);

    const auto write = symbol<void (*)(ExecRegion*, const void*, size_t)>(backend, "bridge_exec_write");
    const auto detach = symbol<void (*)(ExecRegion*)>(backend, "bridge_exec_detach");
    const auto free = symbol<void (*)(ExecRegion*)>(backend, "bridge_exec_free");
    // ENDBR64; mov eax,42; ret. Exercise actual RX execution, then a rewrite
    // through the other alias. No mmap/mprotect is supplied by this fixture.
    uint8_t code[] = {0xf3, 0x0f, 0x1e, 0xfa, 0xb8, 42, 0, 0, 0, 0xc3};
    write(region, code, sizeof(code));
    REQUIRE(std::memcmp(region->begin(), code, sizeof(code)) == 0);
    REQUIRE(std::memcmp(reinterpret_cast<void*>(rw.start), code, sizeof(code)) == 0);
    auto execute = reinterpret_cast<int (*)()>(exec);
    REQUIRE(execute() == 42);
    ExecRegion moved(std::move(*region));
    REQUIRE(region->begin() == nullptr && region->size() == 0);
    *region = std::move(moved);
    code[5] = 73;
    write(region, code, sizeof(code));
    REQUIRE(execute() == 73);
    detach(region);
    REQUIRE(find_mapping(rw.start).start == 0);
    REQUIRE(execute() == 73);
    free(region);
    REQUIRE(find_mapping(exec).start == 0);
    storage.check();
    region->~ExecRegion();
}

int main(int argc, char** argv) {
    REQUIRE(argc >= 2);
    using LoweredCreate = void* (*)(void*, size_t);
    if (argc == 2) {
        const auto hook = symbol<LoweredCreate>(RTLD_DEFAULT,
            "_ZN8berberis26ExecRegionElfBackedFactory6CreateEm");
        GuardedResult early(false);
        if (std::strcmp(argv[1], "missing") == 0) hook(early.output, 1);
        if (std::strcmp(argv[1], "null") == 0) hook(nullptr, 1);
        if (std::strcmp(argv[1], "oversize") == 0) hook(early.output, kCapacity + 1);
        if (std::strcmp(argv[1], "overflow") == 0) hook(early.output, std::numeric_limits<size_t>::max());
        REQUIRE(false);
    }
    REQUIRE(argc == 3);
    const bool fake = std::strcmp(argv[1], "fake") == 0;
    const int flags = std::strcmp(argv[1], "local") == 0 ? RTLD_LOCAL : RTLD_GLOBAL;
    void* backend = dlopen(argv[2], RTLD_NOW | flags);
    if (!backend) std::fprintf(stderr, "backend dlopen: %s\n", dlerror());
    REQUIRE(backend != nullptr);
    const auto make = symbol<LoweredCreate>(backend, "bridge_exec_make");
    const size_t requests[] = {0, 1, 4095, kCapacity - 1, kCapacity};
    for (bool leading : {false, true}) {
        for (size_t requested : requests) {
            GuardedResult result(leading);
            const size_t fds = fd_count();
            REQUIRE(make(result.output, requested) == result.output);
            REQUIRE(fd_count() == fds);
            if (fake) {
                REQUIRE(symbol<size_t (*)()>(backend, "bridge_exec_delegate_size")() == kCapacity);
                check_fake(result,
                    symbol<const uint8_t* (*)()>(backend, "bridge_exec_fake_exec")(),
                    symbol<const uint8_t* (*)()>(backend, "bridge_exec_fake_write")(), kCapacity);
            } else {
                check_real(backend, result);
            }
        }
    }
    if (!fake) {
        // Simultaneous regions must not reuse the same RX mapping.
        GuardedResult first(false), second(true);
        make(first.output, 1);
        make(second.output, 1);
        REQUIRE(first.region()->begin() != second.region()->begin());
        check_real(backend, first);
        check_real(backend, second);
    }
    std::puts(fake ? "Rust hook -> C++ fake delegate: sret, all fields, fixed 512 KiB size, moves and canaries passed"
                   : "unmodified anonymous backend: RX/RW alias rewrites, execution, low-2G, fd closure, detach/free and distinct regions passed");
    REQUIRE(dlclose(backend) == 0);
}
#endif
