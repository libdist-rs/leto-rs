"""Canonical committee → Mysticeti config files.

Mysticeti uses its own committee.yaml + per-authority parameters.yaml.
Field shape depends on the pinned SHA — re-check against the cloned
checkout before a real run.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import yaml

from orchestrator.genconfig import Committee


def translate(committee: Committee, out_dir: Path, protocol: str = "mysticeti") -> dict[str, Path]:
    """Write Mysticeti committee.yaml + parameters.yaml.

    Returns a dict mapping role label → written file path.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    paths: dict[str, Path] = {}

    # Committee shape: authorities indexed by ID with network addresses.
    # Aligns with `mysticeti-core/src/committee.rs` at the pinned SHA;
    # adjust field names per your pin.
    authorities: list[dict[str, Any]] = []
    for m in committee.members:
        authorities.append({
            "id": m.id,
            "network_address": f"{m.endpoint.host}:{m.endpoint.consensus_port}",
            "metrics_address": f"{m.endpoint.host}:{m.endpoint.metrics_port}",
            "public_key": m.pubkey_b64,
        })

    committee_yaml = {
        "epoch": committee.epoch,
        "authorities": authorities,
    }
    committee_path = out_dir / "committee.yaml"
    committee_path.write_text(yaml.safe_dump(committee_yaml, sort_keys=False))
    paths["committee"] = committee_path

    parameters_yaml = {
        # Per-authority benchmark parameters. Exact key names are
        # Mysticeti-version-specific; PINME against the cloned SHA.
        "benchmark_type": {"transaction_size": 512},
        "consensus": {
            "max_proposed_blocks_per_commit_round": 1,
        },
        "metrics": {
            "scrape_interval_ms": 1000,
        },
    }
    parameters_path = out_dir / "parameters.yaml"
    parameters_path.write_text(yaml.safe_dump(parameters_yaml, sort_keys=False))
    paths["parameters"] = parameters_path

    return paths
