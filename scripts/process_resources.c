// Aggregate Darwin process counters. No paths, query text, or process arguments.
// Compile: xcrun clang -O2 -Wall -Wextra -Werror scripts/process_resources.c -o process-resources
#include <errno.h>
#include <inttypes.h>
#include <libproc.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <time.h>

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "Usage: process-resources PID\n");
        return 2;
    }
    char *end;
    errno = 0;
    long number = strtol(argv[1], &end, 10);
    if (errno || *end || number <= 0 || number > INT32_MAX) return 2;
    struct rusage_info_v4 usage = {0};
    struct timespec clock;
    if (proc_pid_rusage((int)number, RUSAGE_INFO_V4, (rusage_info_t *)&usage) != 0
        || clock_gettime(CLOCK_MONOTONIC, &clock) != 0) {
        perror("Process resource counters");
        return 1;
    }
    printf("{\"pid\":%ld,\"sample_seconds\":%.9f,\"start_ticks\":%" PRIu64
           ",\"cpu_nanoseconds\":%" PRIu64 ",\"resident_bytes\":%" PRIu64
           ",\"physical_footprint_bytes\":%" PRIu64 ",\"peak_physical_footprint_bytes\":%" PRIu64
           ",\"pageins\":%" PRIu64 ",\"disk_bytes_read\":%" PRIu64
           ",\"disk_bytes_written\":%" PRIu64 ",\"logical_writes\":%" PRIu64 "}\n",
           number, (double)clock.tv_sec + (double)clock.tv_nsec / 1e9,
           usage.ri_proc_start_abstime, usage.ri_user_time + usage.ri_system_time,
           usage.ri_resident_size, usage.ri_phys_footprint, usage.ri_lifetime_max_phys_footprint,
           usage.ri_pageins, usage.ri_diskio_bytesread, usage.ri_diskio_byteswritten,
           usage.ri_logical_writes);
    return 0;
}
