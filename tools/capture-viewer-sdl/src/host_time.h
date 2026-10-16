#ifndef HOST_TIME_H
#define HOST_TIME_H
#include <stdint.h>
#if defined(_WIN32)
#include <windows.h>
#else
#include <time.h>
#endif
/* Host time in ns, the clock of FT_CLOCK_DOMAIN_HOST_TIME, read as Jackstay
 * reads it. */
static inline uint64_t host_time_ns(void) {
#if defined(_WIN32)
  /* Jackstay's ticks * 1e9 / frequency, split to fit 64 bits. */
  LARGE_INTEGER ticks, frequency;
  QueryPerformanceCounter(&ticks);
  QueryPerformanceFrequency(&frequency);
  uint64_t t = (uint64_t)ticks.QuadPart, f = (uint64_t)frequency.QuadPart;
  return t / f * 1000000000u + t % f * 1000000000u / f;
#else
  struct timespec t;
#ifdef __APPLE__
  clock_gettime(CLOCK_UPTIME_RAW, &t);
#else
  clock_gettime(CLOCK_MONOTONIC, &t);
#endif
  return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
#endif
}
#endif
