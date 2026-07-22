#!/usr/bin/env python3
"""Pair and summarize jemalloc/tcmalloc image benchmark rows."""

import csv
import math
import pathlib
import statistics
import sys
from collections import defaultdict


source_path = pathlib.Path(sys.argv[1])
summary_path = pathlib.Path(sys.argv[2])
rows: dict[tuple[str, int, int], dict[str, list[dict[str, str]]]] = defaultdict(
    lambda: defaultdict(list)
)
failures: list[dict[str, str]] = []
with source_path.open() as source:
    for row in csv.DictReader(source, delimiter="\t"):
        if row["status"] != "PASS":
            failures.append(row)
            continue
        key = (row["protocol"], int(row["payload_bytes"]), int(row["concurrency"]))
        rows[key][row["allocator"]].append(row)


def median(items: list[dict[str, str]], field: str) -> float:
    return statistics.median(float(item[field] or 0) for item in items)


fields = [
    "protocol",
    "payload_bytes",
    "concurrency",
    "jemalloc_rps",
    "tcmalloc_rps",
    "rps_delta_pct",
    "jemalloc_p99_us",
    "tcmalloc_p99_us",
    "p99_delta_pct",
    "jemalloc_cpu_pct",
    "tcmalloc_cpu_pct",
    "jemalloc_rps_per_cpu_pct",
    "tcmalloc_rps_per_cpu_pct",
    "cpu_efficiency_delta_pct",
    "jemalloc_peak_rss_kib",
    "tcmalloc_peak_rss_kib",
    "peak_rss_delta_pct",
]
ratios: dict[str, list[float]] = defaultdict(list)
with summary_path.open("w", newline="") as target:
    writer = csv.DictWriter(target, fieldnames=fields, delimiter="\t")
    writer.writeheader()
    for key in sorted(rows):
        grouped = rows[key]
        if not {"jemalloc", "tcmalloc"}.issubset(grouped):
            continue
        jem = grouped["jemalloc"]
        tcm = grouped["tcmalloc"]
        jrps, trps = median(jem, "rps"), median(tcm, "rps")
        jp99, tp99 = median(jem, "p99_us"), median(tcm, "p99_us")
        jcpu, tcpu = median(jem, "cpu_avg_pct"), median(tcm, "cpu_avg_pct")
        jrss, trss = max(float(item["rss_peak_kib"]) for item in jem), max(
            float(item["rss_peak_kib"]) for item in tcm
        )
        jeff = jrps / jcpu if jcpu else 0.0
        teff = trps / tcpu if tcpu else 0.0
        rps_ratio = trps / jrps if jrps else 0.0
        p99_ratio = tp99 / jp99 if jp99 else 0.0
        eff_ratio = teff / jeff if jeff else 0.0
        rss_ratio = trss / jrss if jrss else 0.0
        if rps_ratio > 0:
            ratios["rps"].append(rps_ratio)
        if p99_ratio > 0:
            ratios["p99"].append(p99_ratio)
        if eff_ratio > 0:
            ratios["efficiency"].append(eff_ratio)
        if rss_ratio > 0:
            ratios["rss"].append(rss_ratio)
        writer.writerow(
            {
                "protocol": key[0],
                "payload_bytes": key[1],
                "concurrency": key[2],
                "jemalloc_rps": f"{jrps:.2f}",
                "tcmalloc_rps": f"{trps:.2f}",
                "rps_delta_pct": f"{(rps_ratio - 1) * 100:.2f}",
                "jemalloc_p99_us": f"{jp99:.0f}",
                "tcmalloc_p99_us": f"{tp99:.0f}",
                "p99_delta_pct": f"{(p99_ratio - 1) * 100:.2f}",
                "jemalloc_cpu_pct": f"{jcpu:.2f}",
                "tcmalloc_cpu_pct": f"{tcpu:.2f}",
                "jemalloc_rps_per_cpu_pct": f"{jeff:.2f}",
                "tcmalloc_rps_per_cpu_pct": f"{teff:.2f}",
                "cpu_efficiency_delta_pct": (
                    f"{(eff_ratio - 1) * 100:.2f}" if eff_ratio else "NA"
                ),
                "jemalloc_peak_rss_kib": f"{jrss:.0f}",
                "tcmalloc_peak_rss_kib": f"{trss:.0f}",
                "peak_rss_delta_pct": (
                    f"{(rss_ratio - 1) * 100:.2f}" if rss_ratio else "NA"
                ),
            }
        )


def geometric_mean(values: list[float]) -> float:
    return math.exp(sum(math.log(value) for value in values) / len(values))


print(f"paired_cases={len(ratios['rps'])} failed_rows={len(failures)}")
if ratios["rps"]:
    print(f"tcmalloc_rps_geomean_delta_pct={(geometric_mean(ratios['rps']) - 1) * 100:.2f}")
    print(f"tcmalloc_p99_median_delta_pct={(statistics.median(ratios['p99']) - 1) * 100:.2f}")
    if ratios["efficiency"]:
        print(
            "tcmalloc_cpu_efficiency_geomean_delta_pct="
            f"{(geometric_mean(ratios['efficiency']) - 1) * 100:.2f}"
        )
    else:
        print("tcmalloc_cpu_efficiency_geomean_delta_pct=NA")
    if ratios["rss"]:
        print(
            "tcmalloc_peak_rss_median_delta_pct="
            f"{(statistics.median(ratios['rss']) - 1) * 100:.2f}"
        )
    else:
        print("tcmalloc_peak_rss_median_delta_pct=NA")
