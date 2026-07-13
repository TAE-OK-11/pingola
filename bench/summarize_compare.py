#!/usr/bin/env python3
"""Summarize paired NGINX and Pingora benchmark rows."""

import csv
import math
import pathlib
import statistics
import sys
from collections import defaultdict


source_path = pathlib.Path(sys.argv[1])
summary_path = pathlib.Path(sys.argv[2])
rows = defaultdict(lambda: defaultdict(list))
failures = []

with source_path.open() as source:
    for row in csv.DictReader(source, delimiter="\t"):
        if row["status"] != "PASS":
            failures.append(row)
            continue
        key = (row["protocol"], int(row["payload_bytes"]), int(row["concurrency"]))
        rows[key][row["proxy"]].append(row)


def median(items, field):
    return statistics.median(float(item[field] or 0) for item in items)


def geometric_mean(values):
    positive = [value for value in values if value > 0]
    return math.exp(sum(math.log(value) for value in positive) / len(positive))


fields = [
    "protocol",
    "payload_bytes",
    "concurrency",
    "nginx_rps",
    "pingora_rps",
    "rps_delta_pct",
    "nginx_p99_us",
    "pingora_p99_us",
    "p99_delta_pct",
    "nginx_cpu_pct",
    "pingora_cpu_pct",
    "nginx_rps_per_cpu_pct",
    "pingora_rps_per_cpu_pct",
    "cpu_efficiency_delta_pct",
    "nginx_peak_rss_kib",
    "pingora_peak_rss_kib",
    "peak_rss_delta_pct",
]
ratios = defaultdict(list)

with summary_path.open("w", newline="") as target:
    writer = csv.DictWriter(target, fieldnames=fields, delimiter="\t")
    writer.writeheader()
    for key in sorted(rows):
        grouped = rows[key]
        if not {"nginx", "pingora"}.issubset(grouped):
            continue
        nginx = grouped["nginx"]
        pingora = grouped["pingora"]
        nginx_rps, pingora_rps = median(nginx, "rps"), median(pingora, "rps")
        nginx_p99, pingora_p99 = median(nginx, "p99_us"), median(pingora, "p99_us")
        nginx_cpu, pingora_cpu = median(nginx, "cpu_avg_pct"), median(
            pingora, "cpu_avg_pct"
        )
        nginx_rss = max(float(item["rss_peak_kib"]) for item in nginx)
        pingora_rss = max(float(item["rss_peak_kib"]) for item in pingora)
        nginx_efficiency = nginx_rps / nginx_cpu if nginx_cpu else 0.0
        pingora_efficiency = pingora_rps / pingora_cpu if pingora_cpu else 0.0
        rps_ratio = pingora_rps / nginx_rps if nginx_rps else 0.0
        p99_ratio = pingora_p99 / nginx_p99 if nginx_p99 else 0.0
        efficiency_ratio = (
            pingora_efficiency / nginx_efficiency if nginx_efficiency else 0.0
        )
        rss_ratio = pingora_rss / nginx_rss if nginx_rss else 0.0
        ratios["rps"].append(rps_ratio)
        ratios["p99"].append(p99_ratio)
        ratios["efficiency"].append(efficiency_ratio)
        ratios["rss"].append(rss_ratio)
        ratios[f"protocol:{key[0]}"].append(rps_ratio)
        writer.writerow(
            {
                "protocol": key[0],
                "payload_bytes": key[1],
                "concurrency": key[2],
                "nginx_rps": f"{nginx_rps:.2f}",
                "pingora_rps": f"{pingora_rps:.2f}",
                "rps_delta_pct": f"{(rps_ratio - 1) * 100:.2f}",
                "nginx_p99_us": f"{nginx_p99:.0f}",
                "pingora_p99_us": f"{pingora_p99:.0f}",
                "p99_delta_pct": f"{(p99_ratio - 1) * 100:.2f}",
                "nginx_cpu_pct": f"{nginx_cpu:.2f}",
                "pingora_cpu_pct": f"{pingora_cpu:.2f}",
                "nginx_rps_per_cpu_pct": f"{nginx_efficiency:.2f}",
                "pingora_rps_per_cpu_pct": f"{pingora_efficiency:.2f}",
                "cpu_efficiency_delta_pct": f"{(efficiency_ratio - 1) * 100:.2f}",
                "nginx_peak_rss_kib": f"{nginx_rss:.0f}",
                "pingora_peak_rss_kib": f"{pingora_rss:.0f}",
                "peak_rss_delta_pct": f"{(rss_ratio - 1) * 100:.2f}",
            }
        )

print(f"paired_cases={len(ratios['rps'])} failed_rows={len(failures)}")
if ratios["rps"]:
    print(
        "pingora_rps_geomean_delta_pct="
        f"{(geometric_mean(ratios['rps']) - 1) * 100:.2f}"
    )
    print(
        "pingora_p99_median_delta_pct="
        f"{(statistics.median(ratios['p99']) - 1) * 100:.2f}"
    )
    print(
        "pingora_cpu_efficiency_geomean_delta_pct="
        f"{(geometric_mean(ratios['efficiency']) - 1) * 100:.2f}"
    )
    print(
        "pingora_peak_rss_median_delta_pct="
        f"{(statistics.median(ratios['rss']) - 1) * 100:.2f}"
    )
    for name in sorted(key for key in ratios if key.startswith("protocol:")):
        print(
            f"{name[9:]}_rps_geomean_delta_pct="
            f"{(geometric_mean(ratios[name]) - 1) * 100:.2f}"
        )
