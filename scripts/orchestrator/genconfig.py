"""Canonical committee generation.

Produces a protocol-agnostic committee description that each
per-protocol translator turns into the native config format. One
committee per benchmark run; pinned by the orchestrator's manifest.
"""

from __future__ import annotations

import json
import secrets
from dataclasses import dataclass, asdict, field
from pathlib import Path
from typing import Optional


# ---------------------------------------------------------------------------
# Canonical committee schema
# ---------------------------------------------------------------------------


@dataclass
class Endpoint:
    """One server's network endpoints. Ports are protocol-independent."""

    host: str                       # IP or hostname (private for AWS)
    consensus_port: int             # server↔server sig/consensus-plane messages
    data_port: int                  # server↔server data-plane (Hera DataPropose/Request/Response)
    mempool_port: int               # client→server raw tx (libmempool-rs path)
    client_port: int                # server↔client wire (ClientMsg, NewBatch)
    metrics_port: int               # Prometheus or similar; protocol-specific


@dataclass
class CommitteeMember:
    """One node in the committee."""

    id: int
    endpoint: Endpoint
    pubkey_b64: str                 # ED25519 verifying key, base64
    role: str = "node"              # "node" or "client"


@dataclass
class Committee:
    """Canonical committee — protocol-agnostic."""

    n: int                          # total node count
    f: int                          # tolerated faults (Byzantine)
    members: list[CommitteeMember]  # nodes only; clients tracked separately
    clients: list[CommitteeMember]  # client driver machines
    epoch: int = 0                  # initial epoch
    crypto: str = "ed25519"         # algorithm identifier

    def to_dict(self) -> dict:
        return {
            "n": self.n,
            "f": self.f,
            "epoch": self.epoch,
            "crypto": self.crypto,
            "members": [asdict(m) for m in self.members],
            "clients": [asdict(c) for c in self.clients],
        }


# ---------------------------------------------------------------------------
# Generation
# ---------------------------------------------------------------------------


def _placeholder_pubkey() -> str:
    """Random 32-byte placeholder for development.

    Real key generation happens per-protocol via that protocol's
    `keys` subcommand (e.g. `node keys -n <N>`); the orchestrator
    invokes it after writing the committee JSON. This placeholder
    keeps the schema typed during local smoke runs where keys are
    generated separately.
    """
    return secrets.token_urlsafe(32)[:43] + "="


def _allocate_ports(base: int = 18000, stride: int = 1000) -> dict[int, Endpoint]:
    """Assign distinct port ranges per node id for local-tmux runs.

    Layout matches the existing in-process harness (consensus=18xxx,
    mempool=19xxx, etc.) so the same generator works for local + AWS.
    """
    # Placeholder; real layout filled in by callers per host.
    return {}


def generate_local(
    n: int,
    f: int,
    num_clients: int,
    base_port: int = 18000,
) -> Committee:
    """Local-tmux committee — all members on 127.0.0.1, distinct ports.

    Used for `fab smoke --target local`. Not for AWS.
    """
    members: list[CommitteeMember] = []
    for i in range(n):
        offset = i * 100
        members.append(
            CommitteeMember(
                id=i,
                endpoint=Endpoint(
                    host="127.0.0.1",
                    consensus_port=base_port + 0 + offset,
                    data_port=base_port + 4 + offset,
                    mempool_port=base_port + 1 + offset,
                    client_port=base_port + 2 + offset,
                    metrics_port=base_port + 3 + offset,
                ),
                pubkey_b64=_placeholder_pubkey(),
                role="node",
            )
        )
    clients: list[CommitteeMember] = []
    client_base = base_port + 10_000
    for c in range(num_clients):
        offset = c * 10
        clients.append(
            CommitteeMember(
                id=n + c,                # client ids start after node ids
                endpoint=Endpoint(
                    host="127.0.0.1",
                    consensus_port=0,    # unused for clients
                    data_port=0,
                    mempool_port=0,
                    client_port=client_base + 0 + offset,
                    metrics_port=0,
                ),
                pubkey_b64=_placeholder_pubkey(),
                role="client",
            )
        )
    return Committee(n=n, f=f, members=members, clients=clients)


def generate_aws(
    node_hosts: list[str],
    client_hosts: list[str],
    f: int,
    base_port: int = 18000,
) -> Committee:
    """AWS committee — one host per member.

    Caller supplies the boto3-provisioned private IPs.
    """
    members: list[CommitteeMember] = [
        CommitteeMember(
            id=i,
            endpoint=Endpoint(
                host=host,
                consensus_port=base_port + 0,
                data_port=base_port + 4,
                mempool_port=base_port + 1,
                client_port=base_port + 2,
                metrics_port=base_port + 3,
            ),
            pubkey_b64=_placeholder_pubkey(),
            role="node",
        )
        for i, host in enumerate(node_hosts)
    ]
    clients: list[CommitteeMember] = [
        CommitteeMember(
            id=len(node_hosts) + i,
            endpoint=Endpoint(
                host=host,
                consensus_port=0,
                data_port=0,
                mempool_port=0,
                client_port=base_port + 100,
                metrics_port=0,
            ),
            pubkey_b64=_placeholder_pubkey(),
            role="client",
        )
        for i, host in enumerate(client_hosts)
    ]
    return Committee(n=len(node_hosts), f=f, members=members, clients=clients)


def write_committee(committee: Committee, out_dir: Path) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / "committee.json"
    path.write_text(json.dumps(committee.to_dict(), indent=2))
    return path
