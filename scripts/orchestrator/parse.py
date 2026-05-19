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
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Iterable, Iterator


_DP_LINE = re.compile(r"DP\[(?P<key>Throughput|Latency)\]\s*[:=]\s*(?P<value>[-+]?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?)")


@dataclass
class Sample:
    """One DP[…] reading from one client process."""

    protocol: str
    n: int
    f: int
    trial: int
    rate_target: float           # offered tx/s (the load level being driven)
    throughput: float            # committed tx/s
    latency_ms: float            # end-to-end client-side latency
    run_id: str                  # results-tag/<protocol>-n<n>-f<f>/run-<trial>
    source_file: str             # path to the client log this sample came from


def parse_log(path: Path) -> dict[str, float | None]:
    """Pull the *last* DP[Throughput] and DP[Latency] from a single log.

    Clients emit DP[…] every `window` seconds and once at shutdown;
    we take the last reading since that reflects the full measurement
    window. Returns `{"throughput": ..., "latency_ms": ...}` with
    `None` for any missing key.
    """
    throughput: float | None = None
    latency: float | None = None
    with path.open("r", errors="replace") as fp:
        for line in fp:
            m = _DP_LINE.search(line)
            if not m:
                continue
            key = m.group("key")
            value = float(m.group("value"))
            if key == "Throughput":
                throughput = value
            elif key == "Latency":
                latency = value
    return {"throughput": throughput, "latency_ms": latency}


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
    seen: list[Path] = []
    for log_path in sorted(run_dir.glob("*.log")):
        readings = parse_log(log_path)
        if readings["throughput"] is not None:
            thrs.append(readings["throughput"])
        if readings["latency_ms"] is not None:
            lats.append(readings["latency_ms"])
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
