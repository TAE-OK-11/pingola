#!/usr/bin/env python3
"""Summarize cgroup CPU and process RSS samples emitted by compare.sh."""

import pathlib
import sys


rows: list[tuple[int, int, int]] = []
for line in pathlib.Path(sys.argv[1]).read_text().splitlines():
    fields = line.split()
    if len(fields) == 3:
        rows.append(tuple(map(int, fields)))
if not rows:
    print("RESOURCE cpu_avg=0 cpu_peak=0 rss_avg_kib=0 rss_peak_kib=0")
    raise SystemExit
elapsed = max(1, rows[-1][0] - rows[0][0])
used = max(0, rows[-1][1] - rows[0][1])
cpu_avg = used * 100.0 / elapsed
peaks = []
for previous, current in zip(rows, rows[1:]):
    interval = current[0] - previous[0]
    if interval > 0:
        peaks.append((current[1] - previous[1]) * 100.0 / interval)
rss = [row[2] for row in rows]
print(
    f"RESOURCE cpu_avg={cpu_avg:.2f} cpu_peak={max(peaks, default=0):.2f} "
    f"rss_avg_kib={sum(rss) / len(rss):.0f} rss_peak_kib={max(rss)}"
)
