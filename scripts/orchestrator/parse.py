"""DP[…] log parser.

Every protocol's client process (or its dpbridge sidecar) emits

  DP[Throughput]: <f64>
  DP[Latency]: <f64>

on stderr. This module turns a directory of `client*.log` files into
one `results.jsonl` row per (run, level) sample, suitable for
`orchestrator/plot.py`.

Format spec lives in `scripts/README.md`. Compatible with the format
libapollo-rs's `consensus::statistics` already emits, so the parser is
single-source across all five protocols.
"""

from __future__ import annotations

import json
import re
import statistics
from dataclasses import dataclass, asdict, field
from pathlib import Path
from typing import Iterable, Iterator


_DP_LINE = re.compile(r"DP\[(?P<key>Throughput|Latency)\]\s*[:=]\s*(?P<value>[-+]?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?)")


@dataclass
class Sample:
    """One DP[…] reading from one client process.

    Carries both the run-level point estimate (`throughput`, `latency_ms`
    — medians) AND the underlying per-emission-window readings
    (`throughputs_raw`, `latencies_raw`) so downstream consumers can
    compute confidence intervals or run statistical tests across windows
    rather than only across trials. Top-tier-conference reviewers
    typically want CIs; preserving the raw stream costs ~few KB per row
    and keeps results.jsonl self-contained.

    `throughputs_raw` / `latencies_raw` are post-warmup-drop (see
    parse_log) and aggregated across all logs in the run dir, in the
    order they were read.
    """

    protocol: str
    n: int
    f: int
    trial: int
    rate_target: float           # offered tx/s (the load level being driven)
    throughput: float            # committed tx/s — median over post-warmup windows
    latency_ms: float            # end-to-end client-side latency — median over windows
    run_id: str                  # results-tag/<protocol>-n<n>-f<f>/run-<trial>
    source_file: str             # path to the client log this sample came from
    throughputs_raw: list[float] = field(default_factory=list)
    latencies_raw: list[float] = field(default_factory=list)


# Number of leading DP windows to drop as "warmup" before computing the
# median.  The orchestrator's `warmup_secs` is observed by the sleep
# *before* launching clients/scrape, but the first emission window of
# the server-side counter or the client-side latency histogram can still
# include partial-buildup data (especially Zeus where the first sig
# round commits an outlier-latency burst).  Dropping window 0 of each
# stream gives a more honest steady-state median.
WARMUP_DROP = 1


def parse_log(path: Path) -> dict:
    """Pull the per-emission-window readings of DP[Throughput] and
    DP[Latency] from a single log, plus their steady-state medians.

    Filters:
    - Drop zero readings ("no data this window" — covers post-client-
      finish windows and pre-load windows).
    - Drop the first WARMUP_DROP non-zero readings per stream (warmup
      transient — Zeus's first commit window is famously high-latency).

    If after warmup-drop fewer than 1 reading remains for a stream,
    the warmup-drop is skipped for that stream (don't lose the data
    for protocols that only emit a few windows, e.g. apollo's closed-
    loop client).

    Returns: {
      "throughput": median or None,
      "latency_ms": median or None,
      "throughputs_raw": post-warmup-drop list (may be empty),
      "latencies_raw":   post-warmup-drop list (may be empty),
    }
    """
    throughputs: list[float] = []
    latencies: list[float] = []
    with path.open("r", errors="replace") as fp:
        for line in fp:
            m = _DP_LINE.search(line)
            if not m:
                continue
            key = m.group("key")
            value = float(m.group("value"))
            if key == "Throughput":
                # KEEP zero-throughput windows. For bursty-commit protocols
                # (e.g., Hera under faults: data plane keeps minting blocks
                # but sig-chain stalls until view-change, then commits the
                # backlog in one burst) the per-window stream is mostly 0
                # with occasional huge spikes. Filtering zeros and taking
                # the median would report ~50k for a real ~5k sustained
                # rate. We keep all windows and take the mean below, so
                # `throughput = sum(committed_txs) / measure_secs`.
                throughputs.append(value)
            elif key == "Latency":
                # Latency 0 isn't emitted unless there's at least one
                # sample in the window; filtering 0 is defensive but
                # rarely fires.
                if value > 0.0:
                    latencies.append(value)

    def _trim_warmup(xs: list[float]) -> list[float]:
        # Drop the first WARMUP_DROP readings if at least one remains
        # after the drop.  Earlier threshold of `len > WARMUP_DROP + 1`
        # was too conservative — Zeus typically emits exactly 2 latency
        # readings (warmup + steady) and we want to drop the warmup,
        # leaving 1 steady-state reading.  Apollo's closed-loop client
        # emits only 1 reading; we keep it (no warmup to drop in a
        # single-window run).
        if len(xs) > WARMUP_DROP:
            return xs[WARMUP_DROP:]
        return xs

    throughputs = _trim_warmup(throughputs)
    latencies = _trim_warmup(latencies)

    return {
        # MEAN, not median, so bursty protocols (sig-chain stalls then
        # catches up) report the true `total_txs / measure_secs` rate.
        # For steady protocols mean ≈ median, so this is a no-op there.
        "throughput": statistics.mean(throughputs) if throughputs else None,
        "latency_ms": statistics.median(latencies) if latencies else None,
        "throughputs_raw": throughputs,
        "latencies_raw": latencies,
    }


def parse_run_dir(
    run_dir: Path,
    protocol: str,
    n: int,
    f: int,
    trial: int,
    rate_target: float,
    run_id: str,
) -> Sample | None:
    """Parse DP[…] across all role logs in a run dir, aggregate by median.

    Convention (post-DP-wiring):
      - throughput → emitted by server on node-0 (`node-0.log`)
      - latency    → emitted by client(s) (`client-*.log`)
      - sidecars   → `sidecar*.log` (Mysticeti dpbridge); throughput.

    Multiple readings per log (per emission window) are read; we take
    the last one as the steady-state value, then median across logs
    (relevant for multi-client runs).
    """
    thrs: list[float] = []
    lats: list[float] = []
    thrs_raw: list[float] = []
    lats_raw: list[float] = []
    seen: list[Path] = []
    for log_path in sorted(run_dir.glob("*.log")):
        readings = parse_log(log_path)
        if readings["throughput"] is not None:
            thrs.append(readings["throughput"])
        if readings["latency_ms"] is not None:
            lats.append(readings["latency_ms"])
        thrs_raw.extend(readings.get("throughputs_raw", []))
        lats_raw.extend(readings.get("latencies_raw", []))
        if readings["throughput"] is not None or readings["latency_ms"] is not None:
            seen.append(log_path)
    if not thrs and not lats:
        return None
    return Sample(
        protocol=protocol,
        n=n,
        f=f,
        trial=trial,
        rate_target=rate_target,
        throughput=statistics.median(thrs) if thrs else float("nan"),
        latency_ms=statistics.median(lats) if lats else float("nan"),
        run_id=run_id,
        source_file=";".join(str(p) for p in seen),
        throughputs_raw=thrs_raw,
        latencies_raw=lats_raw,
    )


def iter_results(results_root: Path) -> Iterator[Sample]:
    """Walk `state/results/<stamp>/<protocol>-n<n>-f<f>/run-<r>/` and
    yield one Sample per run dir.

    Expects the layout the bench runner writes. A `manifest.json` at
    `results_root/manifest.json` carries the rate-target schedule per
    (protocol, n, f); we cross-reference it to attach rate_target to
    each sample. If the manifest is absent or malformed, rate_target
    falls back to NaN.
    """
    manifest_path = results_root / "manifest.json"
    manifest: dict = {}
    if manifest_path.exists():
        try:
            manifest = json.loads(manifest_path.read_text())
        except json.JSONDecodeError:
            manifest = {}
    rate_map: dict[tuple[str, int, int, int], float] = {}
    for entry in manifest.get("runs", []):
        key = (
            entry.get("protocol"),
            int(entry.get("n", -1)),
            int(entry.get("f", -1)),
            int(entry.get("trial", -1)),
        )
        rate_map[key] = float(entry.get("rate_target", float("nan")))

    for protocol_dir in sorted(results_root.iterdir()):
        if not protocol_dir.is_dir():
            continue
        name = protocol_dir.name
        m = re.match(r"(?P<protocol>[a-z_]+)-n(?P<n>\d+)-f(?P<f>\d+)", name)
        if not m:
            continue
        protocol = m.group("protocol")
        n = int(m.group("n"))
        f = int(m.group("f"))
        for run_dir in sorted(protocol_dir.glob("run-*")):
            trial = int(run_dir.name.removeprefix("run-"))
            rate_target = rate_map.get(
                (protocol, n, f, trial), float("nan")
            )
            sample = parse_run_dir(
                run_dir,
                protocol=protocol,
                n=n,
                f=f,
                trial=trial,
                rate_target=rate_target,
                run_id=f"{results_root.name}/{name}/run-{trial}",
            )
            if sample is not None:
                yield sample


def write_jsonl(samples: Iterable[Sample], out: Path) -> int:
    out.parent.mkdir(parents=True, exist_ok=True)
    count = 0
    with out.open("w") as fp:
        for s in samples:
            fp.write(json.dumps(asdict(s)) + "\n")
            count += 1
    return count
