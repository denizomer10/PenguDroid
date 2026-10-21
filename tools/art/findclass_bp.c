/*
 * findclass_bp — log every art::ClassLinker::FindClass descriptor.
 *
 *   findclass_bp <symbol-vaddr-hex> <prog> [args...]
 *
 * The target execs, dlopens libart.so, and then calls
 * `art::ClassLinker::FindClass(Thread*, const char* descriptor, Handle<ClassLoader>)`.
 * A software breakpoint is placed at the given offset from libart.so's load
 * base once the library appears in /proc/<pid>/maps; on each hit the second
 * argument (`rsi`, the descriptor) is read out of the child and printed.
 *
 * Why ptrace: `ClassLinker::FindClass` has local linkage inside libart.so, so
 * it cannot be interposed with `LD_PRELOAD`, and ART throws a *preallocated*
 * NoClassDefFoundError, which carries no message and no stack trace.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/types.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>

static int map_base(pid_t pid, unsigned long *base) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/maps", pid);
    FILE *f = fopen(path, "r");
    if (!f) return -1;
    char line[512];
    int found = 0;
    int have_exec = 0;
    const char *want = getenv("BP_LIB");
    if (!want || !*want) want = "libart.so";
    while (fgets(line, sizeof(line), f)) {
        if (!strstr(line, want)) continue;
        unsigned long start, end, off;
        char perms[8];
        if (sscanf(line, "%lx-%lx %7s %lx", &start, &end, perms, &off) != 4) continue;
        if (strchr(perms, 'x')) have_exec = 1;
        /* The first PT_LOAD has file offset 0; its mapping start is the base. */
        if (off == 0 && !found) {
            *base = start;
            found = 1;
        }
    }
    fclose(f);
    /* Do not arm until the loader has mapped the executable segment: before
     * that the whole file is one read-only mapping and `base + vaddr` still
     * resolves to file offset `vaddr`, which is not the code. */
    if (!found || !have_exec) return -1;
    return 0;
}

static void print_mapping(pid_t pid, unsigned long addr) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/maps", pid);
    FILE *f = fopen(path, "r");
    if (!f) return;
    char line[512];
    char file[256] = {0};
    unsigned long base = 0;
    while (fgets(line, sizeof(line), f)) {
        unsigned long start, end, off;
        char perm[8];
        if (sscanf(line, "%lx-%lx %7s %lx", &start, &end, perm, &off) != 4) continue;
        if (addr >= start && addr < end) {
            fprintf(stderr, "[crash] %#lx in %s", addr, line);
            char *p = strchr(line, '/');
            if (p) {
                strncpy(file, p, sizeof(file) - 1);
                file[strcspn(file, "\n")] = 0;
            }
            break;
        }
    }
    if (!file[0]) {
        fclose(f);
        return;
    }
    /* Find the offset-0 mapping of the same file: that is the load base. */
    rewind(f);
    while (fgets(line, sizeof(line), f)) {
        unsigned long start, end, off;
        char perm[8];
        if (sscanf(line, "%lx-%lx %7s %lx", &start, &end, perm, &off) != 4) continue;
        if (off == 0 && strstr(line, file)) {
            base = start;
            break;
        }
    }
    fclose(f);
    if (base) {
        fprintf(stderr, "[crash]   module base %#lx, symbol offset %#lx\n", base, addr - base);
    }
}

static int read_child(pid_t pid, unsigned long addr, char *out, size_t cap) {
    size_t n = 0;
    while (n + 1 < cap) {
        errno = 0;
        long word = ptrace(PTRACE_PEEKDATA, pid, (void *)addr, NULL);
        if (errno) break;
        for (int i = 0; i < (int)sizeof(long) && n + 1 < cap; i++) {
            char c = (char)((word >> (8 * i)) & 0xff);
            if (c == '\0') {
                out[n] = '\0';
                return 0;
            }
            out[n++] = c;
        }
        addr += sizeof(long);
    }
    out[n] = '\0';
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <sym-vaddr-hex> <prog> [args...]\n", argv[0]);
        return 2;
    }
    unsigned long sym = strtoul(argv[1], NULL, 16);

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, NULL, NULL);
        execvp(argv[2], &argv[2]);
        perror("execvp");
        _exit(127);
    }

    int status;
    if (waitpid(pid, &status, 0) < 0) {
        perror("waitpid");
        return 1;
    }
    fprintf(stderr, "[bp] tracing pid %d for %s+0x%lx\n", pid, argv[2], sym);
    const pid_t main_pid = pid;
    /* Follow every thread: the interesting work happens on threads the guest
     * spawns (the ART helper binds the app from a second thread), and a crash
     * there is invisible to a single-thread tracer. */
    ptrace(PTRACE_SETOPTIONS, pid, NULL, (void *)(long)PTRACE_O_TRACECLONE);

    unsigned long bp = 0;
    long orig = 0;
    int armed = 0;

    /* Run syscall-by-syscall until libart.so is mapped, then set the breakpoint.
     * With `sym == 0` this is a plain crash tracer: no breakpoint, no syscall
     * stepping, so the guest runs at close to full speed. */
    while (1) {
        if (sym != 0 && !armed && map_base(pid, &bp) == 0) {
            bp += sym;
            errno = 0;
            orig = ptrace(PTRACE_PEEKTEXT, pid, (void *)bp, NULL);
            if (errno == 0) {
                long patched = (orig & ~0xffL) | 0xcc;
                if (ptrace(PTRACE_POKETEXT, pid, (void *)bp, (void *)patched) == 0) {
                    armed = 1;
                    fprintf(stderr, "[bp] %s base+0x%lx armed (word=%#lx)\n",
                            getenv("BP_LIB") ? getenv("BP_LIB") : "libart.so",
                            sym, (unsigned long)orig);
                }
            }
        }

        if (WIFEXITED(status) || WIFSIGNALED(status)) {
            if (WIFSIGNALED(status)) {
                fprintf(stderr, "[bp] pid %d died from signal %d\n", pid, WTERMSIG(status));
            }
            /* Other threads exit all the time; only the main process ending
             * means the run is over. */
            if (pid == main_pid) break;
            pid = waitpid(-1, &status, __WALL);
            if (pid < 0) break;
            continue;
        }

        int sig = WSTOPSIG(status);
        int event = status >> 16;
        struct user_regs_struct regs;

        if (event == PTRACE_EVENT_CLONE) {
            unsigned long new_tid = 0;
            ptrace(PTRACE_GETEVENTMSG, pid, NULL, &new_tid);
            fprintf(stderr, "[bp] new thread %lu\n", new_tid);
            ptrace(PTRACE_CONT, pid, NULL, NULL);
        } else if (armed && sig == SIGTRAP
                   && ptrace(PTRACE_GETREGS, pid, NULL, &regs) == 0
                   && (unsigned long)regs.rip == bp + 1) {
            char desc[256];
            const char *which = getenv("BP_REG");
            unsigned long arg = regs.rsi;
            if (which && strcmp(which, "rdx") == 0) arg = regs.rdx;
            else if (which && strcmp(which, "rcx") == 0) arg = regs.rcx;
            else if (which && strcmp(which, "rdi") == 0) arg = regs.rdi;
            else if (which && strcmp(which, "r14") == 0) arg = regs.r14;
            else if (which && strcmp(which, "r15") == 0) arg = regs.r15;
            else if (which && strcmp(which, "rbx") == 0) arg = regs.rbx;
            else if (which && strcmp(which, "r8") == 0) arg = regs.r8;
            else if (which && strcmp(which, "r9") == 0) arg = regs.r9;
            read_child(pid, arg, desc, sizeof(desc));
            fprintf(stderr, "[findclass] %s\n", desc);
            if (getenv("BP_CALLER")) {
                /* The breakpoint fires before the prologue pushes anything, so
                 * the return address is the word at rsp. */
                errno = 0;
                long ra = ptrace(PTRACE_PEEKDATA, pid, (void *)(unsigned long)regs.rsp, NULL);
                fprintf(stderr, "[bp] hit tid %d rip=%#llx rdi=%#llx rsi=%#llx rdx=%#llx rcx=%#llx r8=%#llx r9=%#llx ra=%#lx\n",
                        pid, (unsigned long long)regs.rip, (unsigned long long)regs.rdi,
                        (unsigned long long)regs.rsi, (unsigned long long)regs.rdx,
                        (unsigned long long)regs.rcx, (unsigned long long)regs.r8,
                        (unsigned long long)regs.r9, (unsigned long)ra);
                if (!errno) print_mapping(pid, (unsigned long)ra);
                const char *dump = getenv("BP_DUMP");
                if (dump && *dump) {
                    /* BP_DUMP=<reg>:<bytes> — hex-dump memory at a register. */
                    char reg[16] = {0};
                    unsigned long n = 0;
                    if (sscanf(dump, "%15[^:]:%lu", reg, &n) == 2) {
                        unsigned long a = 0;
                        if (!strcmp(reg, "r12")) a = regs.r12;
                        else if (!strcmp(reg, "rsi")) a = regs.rsi;
                        else if (!strcmp(reg, "rdi")) a = regs.rdi;
                        else if (!strcmp(reg, "rbx")) a = regs.rbx;
                        else if (!strcmp(reg, "r13")) a = regs.r13;
                        else if (!strcmp(reg, "r14")) a = regs.r14;
                        else if (!strcmp(reg, "r15")) a = regs.r15;
                        else if (!strcmp(reg, "rsp")) a = regs.rsp;
                        else if (!strcmp(reg, "r8")) a = regs.r8;
                        else if (!strcmp(reg, "r9")) a = regs.r9;
                        fprintf(stderr, "[bp] dump %s=%#lx\n", reg, a);
                        for (unsigned long i = 0; i < n; i += 8) {
                            errno = 0;
                            long w = ptrace(PTRACE_PEEKDATA, pid, (void *)(a + i), NULL);
                            if (errno) { fprintf(stderr, "  +%#lx <unmapped>\n", i); break; }
                            fprintf(stderr, "  +%#03lx %#018lx\n", i, (unsigned long)w);
                        }
                    }
                }
            }
            /* Restore, rewind, single-step, re-arm. */
            ptrace(PTRACE_POKETEXT, pid, (void *)bp, (void *)orig);
            regs.rip = bp;
            ptrace(PTRACE_SETREGS, pid, NULL, &regs);
            ptrace(PTRACE_SINGLESTEP, pid, NULL, NULL);
            waitpid(pid, &status, __WALL);
            ptrace(PTRACE_POKETEXT, pid, (void *)bp, (void *)((orig & ~0xffL) | 0xcc));
            ptrace(PTRACE_CONT, pid, NULL, NULL);
        } else {
            int deliver = (sig == SIGTRAP || sig == SIGSTOP) ? 0 : sig;
            if (sig == SIGSEGV || sig == SIGBUS || sig == SIGILL || sig == SIGFPE || sig == SIGABRT) {
                struct user_regs_struct r;
                if (ptrace(PTRACE_GETREGS, pid, NULL, &r) == 0) {
                    fprintf(stderr, "[crash] tid %d signal %d rip=%#llx rsp=%#llx rdi=%#llx rsi=%#llx rdx=%#llx\n",
                            pid, sig, (unsigned long long)r.rip, (unsigned long long)r.rsp,
                            (unsigned long long)r.rdi, (unsigned long long)r.rsi,
                            (unsigned long long)r.rdx);
                    /* Dump stack words that look like return addresses. */
                    fprintf(stderr, "[crash] stack:\n");
                    print_mapping(pid, (unsigned long)r.rip);
                    print_mapping(pid, (unsigned long)r.rsi);
                    for (int i = 0; i < 48; i++) {
                        unsigned long sp = (unsigned long)r.rsp + (unsigned long)i * 8;
                        errno = 0;
                        long w = ptrace(PTRACE_PEEKDATA, pid, (void *)sp, NULL);
                        if (errno) break;
                        fprintf(stderr, "  [rsp+%3d] %#lx\n", i * 8, (unsigned long)w);
                        /* Resolve stack words that point into a mapped module:
                         * those are the return addresses of the call chain. */
                        if (getenv("BP_STACKMAP") && (unsigned long)w > 0x10000) {
                            print_mapping(pid, (unsigned long)w);
                        }
                    }
                }
            }
            if (!armed) {
                if (sym == 0) {
                    ptrace(PTRACE_CONT, pid, NULL, deliver ? (void *)(long)deliver : NULL);
                } else {
                    ptrace(PTRACE_SYSCALL, pid, NULL, deliver ? (void *)(long)deliver : NULL);
                }
            } else {
                ptrace(PTRACE_CONT, pid, NULL, deliver ? (void *)(long)deliver : NULL);
            }
        }
        /* __WALL: report stops from every traced thread, not just the leader. */
        pid = waitpid(-1, &status, __WALL);
        if (pid < 0) break;
    }
    return 0;
}
