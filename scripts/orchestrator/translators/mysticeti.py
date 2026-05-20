"""Canonical committee -> Mysticeti config files.

Invokes `mysticeti benchmark-genesis --ips <ip> ... --working-directory
<out_dir>/mysticeti` to materialise committee.yaml, parameters.yaml, and
per-authority private/<i>.yaml files for a real multi-host deployment.
Returns a dict mapping logical names to the produced artifact paths.
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

from orchestrator.genconfig import Committee


_REPO_ROOT = Path(__file__).resolve().parents[3]
_MYSTICETI_BIN = (
    _REPO_ROOT
    / "scripts"
    / "state"
    / "repos"
    / "mysticeti"
    / "target"
    / "release"
    / "mysticeti"
)


def translate(
    committee: Committee, out_dir: Path, protocol: str = "mysticeti"
) -> dict[str, Path]:
    """Run `mysticeti benchmark-genesis` and return the produced artifact paths."""
    if not _MYSTICETI_BIN.exists():
        raise FileNotFoundError(
            f"mysticeti binary not found at {_MYSTICETI_BIN}. "
            "Build it locally with `cargo build --release --bin mysticeti` "
            "inside scripts/state/repos/mysticeti/ "
            "(fab install/fab build populates this path on AWS hosts but "
            "NOT locally, so local sweeps require a manual build)."
        )

    out_dir.mkdir(parents=True, exist_ok=True)
    work_dir = out_dir / "mysticeti"
    if work_dir.exists():
        shutil.rmtree(work_dir)
    work_dir.mkdir(parents=True)

    ips = [m.endpoint.host for m in sorted(committee.members, key=lambda m: m.id)]
    if len(ips) != committee.n:
        raise ValueError(
            f"committee.n={committee.n} but got {len(ips)} member IPs"
        )

    # Mysticeti's clap binding for `--ips` is `value_delimiter=' ', num_args(4..)`,
    # so all IPs are passed as positional values after a single `--ips` flag.
    cmd: list[str] = [str(_MYSTICETI_BIN), "benchmark-genesis", "--ips", *ips,
                      "--working-directory", str(work_dir)]

    try:
        subprocess.run(cmd, check=True, capture_output=True, text=True)
    except subprocess.CalledProcessError as e:
        raise RuntimeError(
            f"mysticeti benchmark-genesis failed (exit {e.returncode}):\n"
            f"stderr:\n{e.stderr}"
        ) from e

    artifacts: dict[str, Path] = {
        "committee": work_dir / "committee.yaml",
        "parameters": work_dir / "parameters.yaml",
    }
    for i in range(committee.n):
        artifacts[f"private_{i}"] = work_dir / "private" / f"{i}.yaml"

    missing = [str(p) for p in artifacts.values() if not p.exists()]
    if missing:
        raise FileNotFoundError(
            "mysticeti benchmark-genesis succeeded but expected artifacts "
            f"are missing (likely a Mysticeti version mismatch): {missing}"
        )

    return artifacts
