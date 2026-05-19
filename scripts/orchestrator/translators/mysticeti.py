"""Canonical committee → Mysticeti config files.

Two modes:

1. **dry-run** (default for local smoke + benchmarks at small n):
   Mysticeti's `dry-run` subcommand self-generates committee + keys +
   parameters; no translator output is needed.  This function returns
   an empty dict in that case.

2. **benchmark-genesis** (for explicit committee + multi-host runs):
   Invoke `mysticeti benchmark-genesis --ips <list> --working-directory
   <out>` to materialise committee.yaml + parameters.yaml + private
   configs.  Not yet wired here — the dry-run path covers everything
   we need for the Lean+ paper's n ≤ 20 sweep.

Field shape of committee.yaml is Mysticeti-version-specific (the pinned
SHA's `mysticeti_core::committee::Committee` Rust struct).  Whatever we
write here would drift on every Mysticeti commit; subprocess-calling
`benchmark-genesis` when needed is the right answer when this gets
extended.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from orchestrator.genconfig import Committee


def translate(committee: Committee, out_dir: Path, protocol: str = "mysticeti") -> dict[str, Path]:
    """No-op for dry-run mode (the default and only mode currently wired).

    Returns an empty dict; the orchestrator's deploy layer launches
    Mysticeti via `dry-run --committee-size N --authority I` with no
    external config files needed.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    # TODO(mysticeti-benchmark-genesis): when we move beyond dry-run,
    # subprocess `mysticeti benchmark-genesis --ips ... --working-directory
    # <out_dir>` and return the paths it writes.
    _ = committee  # currently unused; bench-genesis will consume it
    return {}
