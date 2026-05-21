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

# Must match deploy.REMOTE_ROOT + "/run".  The validator's storage_path is
# rewritten to <_REMOTE_RUN_PREFIX>/mysticeti/val-<i> (deploy mkdirs it).
_REMOTE_RUN_PREFIX = "/home/ec2-user/leto-bench/run"


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

    # AWS post-processing: benchmark-genesis bakes the orchestrator's
    # LOCAL absolute path into each `private/<i>.yaml` (`storage_path:
    # <local_dir>/private/val-<i>`). Shipping that file to AWS makes
    # mysticeti panic at startup with "Failed to open wal file: NotFound"
    # because the local path doesn't exist on the remote host.  Detect
    # AWS mode (any non-loopback IP) and rewrite each private/<i>.yaml's
    # storage_path to the path deploy.launch_remote actually mkdirs on
    # the AWS host: `<REMOTE_RUN_PREFIX>/mysticeti/val-<i>`.
    is_aws = any(
        m.endpoint.host not in ("127.0.0.1", "localhost")
        for m in committee.members
    )
    if is_aws:
        for i in range(committee.n):
            yml = work_dir / "private" / f"{i}.yaml"
            text = yml.read_text()
            # YAML is exactly two lines: authority_index + storage_path.
            # Rewrite the latter without pulling in a YAML dep.
            new_lines = []
            for ln in text.splitlines():
                if ln.startswith("storage_path:"):
                    new_lines.append(
                        f"storage_path: {_REMOTE_RUN_PREFIX}/mysticeti/val-{i}"
                    )
                else:
                    new_lines.append(ln)
            yml.write_text("\n".join(new_lines) + "\n")

    return artifacts
