"""Canonical committee → libapollo-rs config files.

Apollo and Artemis share libapollo-rs's `config::node::Config` +
`config::client::Config` JSON shapes — see
`~/Github/libapollo-rs/config/src/`. They take the same committee for
both protocols.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from orchestrator.genconfig import Committee


def translate(committee: Committee, out_dir: Path, protocol: str = "apollo") -> dict[str, Path]:
    """Write apollo/artemis server + client configs.

    Returns a dict mapping role label → written file path.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    paths: dict[str, Path] = {}

    # libapollo-rs's node Config (from the genconfig output structure).
    # Field names mirror config/src/node.rs / config/src/client.rs.
    nodes = []
    for m in committee.members:
        nodes.append({
            "id": m.id,
            "ip": m.endpoint.host,
            "consensus_port": m.endpoint.consensus_port,
            "mempool_port": m.endpoint.mempool_port,
            "client_port": m.endpoint.client_port,
            "pubkey": m.pubkey_b64,
        })

    server_settings: dict[str, Any] = {
        "protocol": protocol,
        "num_nodes": committee.n,
        "num_faults": committee.f,
        "delta": 50,              # apollo default per Explore survey
        "block_size": 400,        # txs per block (apollo default)
        "payload": 0,             # tx payload bytes
        "crypto_alg": "ED25519",
        "nodes": nodes,
        "bench_emit_window_secs": 5,
        "bench_metrics_node": 0,
    }

    server_path = out_dir / f"{protocol}-server.json"
    server_path.write_text(json.dumps(server_settings, indent=2))
    paths["server"] = server_path

    for client in committee.clients:
        client_settings: dict[str, Any] = {
            "protocol": protocol,
            "client_id": client.id,
            "total_txs": 50_000,
            "window": 10_000,
            "block_size": 400,
            "payload": 0,
            "nodes": nodes,
            "bench_emit_window_secs": 5,
        }
        client_path = out_dir / f"{protocol}-client-{client.id}.json"
        client_path.write_text(json.dumps(client_settings, indent=2))
        paths[f"client-{client.id}"] = client_path

    return paths
