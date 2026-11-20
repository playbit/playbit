// macOS process counters for the real packaged benchmark. No UI automation.
#include <errno.h>
#include <inttypes.h>
#include <libproc.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

static uint64_t clock_ns(clockid_t clock) {
    struct timespec time;
    if (clock_gettime(clock, &time)) {
        perror("clock_gettime");
        exit(1);
    }
    return (uint64_t)time.tv_sec * 1000000000 + (uint64_t)time.tv_nsec;
}

static uint64_t cpu_ns(uint64_t ticks) {
    mach_timebase_info_data_t scale;
    if (mach_timebase_info(&scale) != KERN_SUCCESS || !scale.denom) {
        fputs("Mach clock conversion unavailable\n", stderr);
        exit(1);
    }
    return (uint64_t)((__uint128_t)ticks * scale.numer / scale.denom);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fputs("Usage: process-resources PID (0 checks counter units on this process)\n", stderr);
        return 2;
    }
    char *end;
    errno = 0;
    long pid = strtol(argv[1], &end, 10);
    if (errno || *end || end == argv[1] || pid < 0 || pid > 2147483647) {
        fputs("Invalid process ID\n", stderr);
        return 2;
    }
    struct rusage_info_v4 before = {0}, after = {0};
    if (!pid) {
        pid = getpid();
        if (proc_pid_rusage((int)pid, RUSAGE_INFO_V4, (rusage_info_t *)&before)) {
            perror("proc_pid_rusage");
            return 1;
        }
        uint64_t start = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
        while (clock_ns(CLOCK_PROCESS_CPUTIME_ID) - start < 50000000) {}
        uint64_t elapsed = clock_ns(CLOCK_PROCESS_CPUTIME_ID) - start;
        if (proc_pid_rusage((int)pid, RUSAGE_INFO_V4, (rusage_info_t *)&after)) {
            perror("proc_pid_rusage");
            return 1;
        }
        uint64_t cpu = cpu_ns(after.ri_user_time - before.ri_user_time
            + after.ri_system_time - before.ri_system_time);
        if (cpu < elapsed * 9 / 10 || cpu > elapsed * 11 / 10) {
            fputs("CPU counter units do not match nanosecond process clock\n", stderr);
            return 1;
        }
    }
    if (proc_pid_rusage((int)pid, RUSAGE_INFO_V4, (rusage_info_t *)&after)) {
        perror("proc_pid_rusage");
        return 1;
    }
    mach_timebase_info_data_t scale;
    if (mach_timebase_info(&scale) != KERN_SUCCESS || !scale.denom) return 1;
    printf("{\"clock\":\"mach_absolute_time\",\"timebase_numer\":%u,\"timebase_denom\":%u,\"observed_ns\":%" PRIu64 ",\"cpu_ns\":%" PRIu64
        ",\"interrupt_wakeups\":%" PRIu64 ",\"package_idle_wakeups\":%" PRIu64
        ",\"resident_bytes\":%" PRIu64 ",\"footprint_bytes\":%" PRIu64
        ",\"lifetime_peak_footprint_bytes\":%" PRIu64 ",\"read_bytes\":%" PRIu64
        ",\"written_bytes\":%" PRIu64 "}\n",
        scale.numer, scale.denom, cpu_ns(mach_absolute_time()), cpu_ns(after.ri_user_time + after.ri_system_time),
        after.ri_interrupt_wkups, after.ri_pkg_idle_wkups, after.ri_resident_size,
        after.ri_phys_footprint, after.ri_lifetime_max_phys_footprint,
        after.ri_diskio_bytesread, after.ri_diskio_byteswritten);
    return 0;
}
