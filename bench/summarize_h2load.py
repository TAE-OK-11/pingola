#!/usr/bin/env python3
"""Print latency/status summary from h2load --log-file output."""

import math
import pathlib
import sys


def percentile(values: list[int], pct: float) -> int:
    if not values:
        return 0
    index = max(0, math.ceil(len(values) * pct / 100.0) - 1)
    return values[min(index, len(values) - 1)]


path = pathlib.Path(sys.argv[1])
latencies: list[int] = []
statuses: dict[int, int] = {}
incomplete = 0
for line in path.read_text().splitlines():
    fields = line.split("\t")
    if len(fields) < 3:
        continue
    status = int(fields[1])
    elapsed = int(fields[2])
    if 100 <= status <= 599:
        statuses[status] = statuses.get(status, 0) + 1
        latencies.append(elapsed)
    else:
        # Timing-based h2load runs can stop with a few in-flight requests at
        # the exact duration boundary. They have status 0 and are neither a
        # completed HTTP response nor an h2load-reported failed stream.
        incomplete += 1
latencies.sort()
status_text = ",".join(f"{status}:{count}" for status, count in sorted(statuses.items()))
print(
    "LATENCY_US "
    + " ".join(
        f"{name}={percentile(latencies, pct)}"
        for name, pct in (
            ("p50", 50),
            ("p90", 90),
            ("p95", 95),
            ("p99", 99),
            ("p999", 99.9),
            ("max", 100),
        )
    )
    + f" statuses={status_text} incomplete={incomplete}"
)
