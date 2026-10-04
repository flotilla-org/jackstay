#ifndef VIEWER_AFFORDANCES_H
#define VIEWER_AFFORDANCES_H
#include "jackstay_affordances.h"
typedef struct { ft_affordances_host *host; int closed; } viewer_affordances;
int viewer_affordances_poll(viewer_affordances *affordances, int log_snapshots);
int viewer_affordances_close(viewer_affordances *affordances);
#endif
