"""Canonical committee → leto-rs config files.

Aligned with `examples/server.json` and `examples/client.json` plus
the post-DP-wiring additions (`consensus_client_port`,
`bench_emit_window_secs`, `bench_metrics_node`, client
`my_confirmation_*` and `confirmation_port`).

Both `node` (Leto) and `node-zeus` (Zeus) consume the same
`consensus::server::Settings`; the binary picks the protocol, the
client side picks `ClientMode::{LetoBroadcast, ZeusEleaderOnly}`.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from orchestrator.genconfig import Committee


def translate(committee: Committee, out_dir: Path, protocol: str = "leto") -> dict[str, Path]:
    """Write the leto-rs server settings JSON + per-client settings.

    Returns a dict mapping role label → written file path.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    paths: dict[str, Path] = {}

    # Server-side parties: shape matches consensus/src/server/settings.rs::Party.
    # IMPORTANT: client_port (libmempool's raw-Tx receiver) and
    # consensus_client_port (consensus's ClientMsg<Tx>::NewBatch receiver)
    # MUST be distinct — sharing the port causes libmempool to receive
    # ClientMsg bytes and fail to deserialize as raw Tx, which (a) spams
    # "Client receiver deserialization error" and (b) starves the consensus
    # NewBatch path so commits never accumulate unique proposers.
    # Match the examples/server.json layout: consensus_client_port =
    # client_port + 4 (gives 4-node range room).
    server_parties: dict[str, dict[str, Any]] = {}
    for m in committee.members:
        server_parties[str(m.id)] = {
            "id": m.id,
            "mempool_address": m.endpoint.host,
            "mempool_port": m.endpoint.mempool_port,
            "consensus_address": m.endpoint.host,
            "consensus_port": m.endpoint.consensus_port,
            "client_port": m.endpoint.client_port,
            "consensus_client_port": m.endpoint.client_port + 4,
        }

    server_settings: dict[str, Any] = {
        "committee_config": {
            "parties": server_parties,
        },
        "mempool_config": {
            "gc_depth": 5,
            "sync_retry_delay": {"secs": 1, "nanos": 0},
            "sync_retry_nodes": committee.n,
        },
        "storage": {
            "base": ".",
            "prefix": "db",
        },
        "bench_config": {
            "batch_size": 500_000,
            "batch_timeout": {"secs": 0, "nanos": 100_000_000},   # 100 ms
            # Sig-chain round-timer (Δ). Must exceed data_timer_duration_ms
            # so the freshness-rule rleader has a chance to receive a fresh
            # eleader block before the round times out into a blame.  With
            # data_timer=1000ms, a sig Δ of ~2000ms gives 2× slack.  Too
            # low a Δ → constant BlameQC rotation → no chain extension →
            # no Zeus commits → no DP throughput readings.
            "delay_in_ms": 2000,
            "eleader_pipeline_depth": 16,
            "data_timer_duration_ms": 1000,
            "bench_emit_window_secs": 5,
            "bench_metrics_node": 0,
        },
    }

    server_path = out_dir / f"{protocol}-server.json"
    server_path.write_text(json.dumps(server_settings, indent=2))
    paths["server"] = server_path

    # Client-side parties: shape matches consensus/src/client/settings.rs::Party.
    # `port` is the server's consensus_client_port (where NewBatch is sent).
    client_parties: dict[str, dict[str, Any]] = {}
    for m in committee.members:
        client_parties[str(m.id)] = {
            "id": m.id,
            "address": m.endpoint.host,
            "port": m.endpoint.client_port + 4,        # = server's consensus_client_port
            "confirmation_port": 0,                    # unused on server-party records
        }

    # One settings file per client driver machine. Each has its own
    # `my_confirmation_port` so multiple clients on one box don't collide.
    if protocol == "leto":
        client_mode: Any = "LetoBroadcast"
    elif protocol == "zeus":
        # eleader for epoch=1 in canonical 0-indexed eleader(epoch, n) =
        # epoch % n is the convention (verify against chain_state/data_chain.rs).
        client_mode = {"ZeusEleaderOnly": {"eleader_id": 0}}
    else:
        client_mode = "LetoBroadcast"

    for client in committee.clients:
        client_settings: dict[str, Any] = {
            "consensus_config": {
                "parties": client_parties,
            },
            "bench_config": {
                "tx_size": 512,
                "burst_interval_ms": 100,
                "txs_per_burst": 488,             # ~250 KB at 512B/tx
                "bench_emit_window_secs": 5,
                "emit_dp": True,
            },
            "client_mode": client_mode,
            "my_confirmation_address": "0.0.0.0",
            "my_confirmation_port": client.endpoint.client_port,
        }
        client_path = out_dir / f"{protocol}-client-{client.id}.json"
        client_path.write_text(json.dumps(client_settings, indent=2))
        paths[f"client-{client.id}"] = client_path

    return paths
