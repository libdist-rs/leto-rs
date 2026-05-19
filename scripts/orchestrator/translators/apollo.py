"""Canonical committee → libapollo-rs config files.

Apollo and Artemis both consume libapollo-rs's `genconfig` output:
- nodes-{i}.json       per-node config (peer list, ports, TLS material refs)
- node-{i}.{chain,key}.pem  per-node TLS material
- client.json          single client config
- client-0.{chain,key}.pem  client TLS
- root-cert.pem        CA root

Plus two files this translator writes itself (genconfig doesn't):
- ip_file              `<ip>:<base_port+i>` per node, one line each
- cli_ip_file          `<ip>:<client_base_port+i>` per node, one line each

This translator subprocess-calls the `genconfig` binary from a built
checkout of libapollo-rs so the JSON schema is always exactly what
`config::Node` expects.  Trying to re-implement the schema in Python
would drift on every libapollo-rs change.

Local dev: looks for `genconfig` at `~/Github/libapollo-rs/target/release/`.
AWS path: install_remote builds it inside `~/leto-bench/repos/libapollo-rs/`;
the deploy layer points GENCONFIG_OVERRIDE at that location.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path
from typing import Any

from orchestrator.genconfig import Committee


# Default discovery: dev-machine path.  Override via env var for AWS.
DEFAULT_GENCONFIG = Path.home() / "Github" / "libapollo-rs" / "target" / "release" / "genconfig"


def _find_genconfig() -> Path:
    override = os.environ.get("GENCONFIG_OVERRIDE")
    if override:
        p = Path(override)
        if p.exists():
            return p
        raise FileNotFoundError(f"GENCONFIG_OVERRIDE={override} not found")
    if DEFAULT_GENCONFIG.exists():
        return DEFAULT_GENCONFIG
    raise FileNotFoundError(
        f"genconfig not found at {DEFAULT_GENCONFIG}; "
        f"build libapollo-rs first: "
        f"`cd ~/Github/libapollo-rs && cargo build --release --bin genconfig`"
    )


def translate(committee: Committee, out_dir: Path, protocol: str = "apollo") -> dict[str, Path]:
    """Generate apollo/artemis configs for the canonical committee.

    Returns {label → path} for the generated files.  Protocol is the
    binary key ("apollo" or "artemis") — both protocols read identical
    genconfig output; the protocol differs only in which binary you
    invoke.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    genconfig = _find_genconfig()

    # Apollo's genconfig allocates ports as `base + node_id`, NOT the
    # canonical committee's stride-100-per-node layout.  Picking four
    # disjoint base-port ranges keeps them clear of leto/zeus's 18000+
    # range so apollo + leto can coexist if both happen to run.
    if not committee.members:
        raise ValueError("committee has no members")
    base_port = 14000                            # -P, node-node consensus
    client_base = 15000                          # -C, server's client-tx port
    mempool_base = 16000                         # -M, mempool peer-sync
    client_listen = 17000                        # -L, client's listen port for pushed ClientMsg

    # node_ips / client_ips: comma-separated unique IPs of all hosts
    # (used for TLS cert SAN).  Local-tmux all share 127.0.0.1.
    node_ips = ",".join(sorted({m.endpoint.host for m in committee.members}))
    client_ips = ",".join(sorted({c.endpoint.host for c in committee.clients})) or "127.0.0.1"

    cmd = [
        str(genconfig),
        "-n", str(committee.n),
        "-f", str(committee.f),
        "-d", "50",                              # Δ in ms
        "-b", "400",                             # block_size (txs/block)
        "-P", str(base_port),
        "-C", str(client_base),
        "-M", str(mempool_base),
        "-L", str(client_listen),
        "--node_ips", node_ips,
        "--client_ips", client_ips,
        "--payload", "0",
        "--target", str(out_dir),
    ]
    subprocess.run(cmd, check=True)

    # Hand-write ip_file + cli_ip_file (genconfig doesn't emit these).
    # Each line: "<ip>:<base_port + i>" for i in 0..n.
    ip_lines = [
        f"{committee.members[i].endpoint.host}:{base_port + i}"
        for i in range(committee.n)
    ]
    cli_lines = [
        f"{committee.members[i].endpoint.host}:{client_base + i}"
        for i in range(committee.n)
    ]
    (out_dir / "ip_file").write_text("\n".join(ip_lines) + "\n")
    (out_dir / "cli_ip_file").write_text("\n".join(cli_lines) + "\n")

    # Build the return dict pointing at the generated files.
    paths: dict[str, Path] = {
        "ip_file": out_dir / "ip_file",
        "cli_ip_file": out_dir / "cli_ip_file",
        "client_config": out_dir / "client.json",
        "root_cert": out_dir / "root-cert.pem",
    }
    for i in range(committee.n):
        paths[f"node_config_{i}"] = out_dir / f"nodes-{i}.json"
        paths[f"node_chain_{i}"] = out_dir / f"node-{i}.chain.pem"
        paths[f"node_key_{i}"] = out_dir / f"node-{i}.key.pem"
    paths["client_chain"] = out_dir / "client-0.chain.pem"
    paths["client_key"] = out_dir / "client-0.key.pem"

    # Verify everything genconfig was supposed to produce actually exists.
    for label, p in paths.items():
        if not p.exists():
            raise RuntimeError(f"genconfig did not produce {label} at {p}")

    return paths
