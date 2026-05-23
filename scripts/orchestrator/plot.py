"""Figure rendering — Pareto, scaling, crash-fault.

Reads results.jsonl (one row per (protocol, n, f, trial, load)), groups
appropriately, plots median + IQR per protocol.
"""

from __future__ import annotations

import json
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Iterable

# matplotlib is imported lazily so the module can be loaded for type-
# checking on machines without the dep.


def _load_jsonl(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def _agg_median_iqr(values: list[float]) -> tuple[float, float, float]:
    if not values:
        return float("nan"), float("nan"), float("nan")
    sorted_v = sorted(values)
    n = len(sorted_v)
    median = statistics.median(sorted_v)
    q1 = sorted_v[max(0, n // 4)]
    q3 = sorted_v[min(n - 1, (3 * n) // 4)]
    return median, q1, q3


def render_pareto(results_jsonl: Path, out: Path) -> None:
    """Throughput-Latency Pareto: x = throughput, y = latency.

    One line per protocol; points sorted by offered rate. Markers at the
    per-load median throughput/latency; error bars span Q1..Q3 across trials
    on both axes.  Output format follows `out` suffix (.pdf, .png, ...).
    """
    import matplotlib.pyplot as plt   # noqa: E402

    rows = _load_jsonl(results_jsonl)
    by_protocol: dict[str, dict[int, list[tuple[float, float]]]] = defaultdict(lambda: defaultdict(list))
    for r in rows:
        by_protocol[r["protocol"]][int(r["rate_target"])].append(
            (r["throughput"], r["latency_ms"])
        )

    fig, ax = plt.subplots(figsize=(8, 5))
    for protocol, by_rate in sorted(by_protocol.items()):
        xs, ys, xerr_lo, xerr_hi, yerr_lo, yerr_hi = [], [], [], [], [], []
        for rate in sorted(by_rate):
            samples = by_rate[rate]
            thrs = [s[0] for s in samples if s[0] == s[0]]   # filter NaN
            lats = [s[1] for s in samples if s[1] == s[1]]
            if not thrs or not lats:
                continue
            t_med, t_q1, t_q3 = _agg_median_iqr(thrs)
            l_med, l_q1, l_q3 = _agg_median_iqr(lats)
            xs.append(t_med); ys.append(l_med)
            xerr_lo.append(t_med - t_q1); xerr_hi.append(t_q3 - t_med)
            yerr_lo.append(l_med - l_q1); yerr_hi.append(l_q3 - l_med)
        if not xs:
            continue
        ax.errorbar(
            xs, ys,
            xerr=[xerr_lo, xerr_hi],
            yerr=[yerr_lo, yerr_hi],
            marker="o", capsize=3, linewidth=1.2, elinewidth=0.8,
            label=protocol,
        )
    ax.set_xlabel("Committed throughput (tx/s)")
    ax.set_ylabel("Latency p50 (ms)")
    ax.set_title(f"Throughput-Latency Pareto — {results_jsonl.parent.name}")
    ax.legend()
    ax.grid(True, alpha=0.3)
    out.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out, bbox_inches="tight")
    plt.close(fig)


def render_scaling(results_jsonl: Path, out: Path) -> None:
    """Scaling: x = n, y = throughput, one line per protocol.

    Uses the per-(protocol, n) sample at each scale (one load point per
    scale in the scalability sweep).
    """
    import matplotlib.pyplot as plt

    rows = _load_jsonl(results_jsonl)
    by_protocol: dict[str, dict[int, list[float]]] = defaultdict(lambda: defaultdict(list))
    for r in rows:
        if r["throughput"] != r["throughput"]:
            continue
        by_protocol[r["protocol"]][int(r["n"])].append(r["throughput"])

    fig, ax = plt.subplots(figsize=(8, 5))
    for protocol, by_n in sorted(by_protocol.items()):
        xs, medians, yerr_lo, yerr_hi = [], [], [], []
        for n in sorted(by_n):
            med, q1, q3 = _agg_median_iqr(by_n[n])
            xs.append(n)
            medians.append(med)
            yerr_lo.append(med - q1)
            yerr_hi.append(q3 - med)
        ax.errorbar(
            xs, medians,
            yerr=[yerr_lo, yerr_hi],
            marker="s", capsize=3, linewidth=1.2, elinewidth=0.8,
            label=protocol,
        )
    ax.set_xlabel("Committee size n")
    ax.set_ylabel("Committed throughput (tx/s)")
    ax.set_xscale("log")
    ax.set_title(f"Scaling — {results_jsonl.parent.name}")
    ax.legend()
    ax.grid(True, alpha=0.3, which="both")
    out.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out, bbox_inches="tight")
    plt.close(fig)


def render_crash_table(results_jsonl: Path, out: Path) -> None:
    """Crash-fault: one CSV-style summary table per protocol with
    median throughput + latency + recovery status."""
    rows = _load_jsonl(results_jsonl)
    by_protocol: dict[str, list[dict]] = defaultdict(list)
    for r in rows:
        by_protocol[r["protocol"]].append(r)
    lines = ["protocol,throughput_med,latency_med,trials"]
    for protocol, rs in sorted(by_protocol.items()):
        thrs = [r["throughput"] for r in rs if r["throughput"] == r["throughput"]]
        lats = [r["latency_ms"] for r in rs if r["latency_ms"] == r["latency_ms"]]
        thr_med = statistics.median(thrs) if thrs else float("nan")
        lat_med = statistics.median(lats) if lats else float("nan")
        lines.append(f"{protocol},{thr_med:.2f},{lat_med:.2f},{len(rs)}")
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text("\n".join(lines) + "\n")
