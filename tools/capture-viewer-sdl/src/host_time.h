#ifndef HOST_TIME_H
#define HOST_TIME_H
#include <stdint.h>
#include <time.h>
/* Host time in ns, the clock of FT_CLOCK_DOMAIN_HOST_TIME. */
static inline uint64_t host_time_ns(void) {
  struct timespec t;
#ifdef __APPLE__
  clock_gettime(CLOCK_UPTIME_RAW, &t);
#else
  clock_gettime(CLOCK_MONOTONIC, &t);
#endif
  return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}
#endif
