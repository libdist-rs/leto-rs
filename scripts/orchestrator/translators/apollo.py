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

AWS post-processing:
genconfig always writes `127.0.0.1` in net_map / mempool_net_map / client_net_map
and embeds local absolute cert paths. When committee members have non-loopback
IPs (AWS mode), _fixup_aws_configs() rewrites those fields in-place so the
pushed configs work on the remote hosts.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path
from typing import Any

from orchestrator.genconfig import Committee


# Default discovery: dev-machine path.  Override via env var for AWS.
DEFAULT_GENCONFIG = Path.home() / "Github" / "libapollo-rs" / "target" / "release" / "genconfig"

# Remote directory where deploy.launch_remote pushes config + cert files.
# Must match deploy.REMOTE_ROOT + "/run/<protocol>/".
_REMOTE_RUN_PREFIX = "/home/ec2-user/leto-bench/run"


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


def _fixup_aws_configs(
    out_dir: Path,
    committee: Committee,
    base_port: int,
    mempool_base: int,
    client_listen: int,
    protocol: str,
) -> None:
    """Post-process genconfig output for AWS deployment.

    genconfig always writes 127.0.0.1 in net_map fields and local
    absolute paths for TLS certs.  This function rewrites:
    - net_map[i]         → committee.members[i].host:base_port+i
    - mempool_net_map[i] → committee.members[i].host:mempool_base+i
    - client_net_map[i]  → committee.clients[i].host:client_listen+i
    - my_cert_path       → <remote_run_dir>/<filename>
    - my_cert_key_path   → <remote_run_dir>/<filename>
    - root_cert_path     → <remote_run_dir>/<filename>
    - my_listen_addr     → 0.0.0.0:<port>  (client.json)

    Only applied when any member IP is non-loopback.
    """
    # Build id→ip maps for nodes and clients.
    node_ip = {m.id: m.endpoint.host for m in committee.members}
    client_ip = {c.id: c.endpoint.host for c in committee.clients}
    remote_dir = f"{_REMOTE_RUN_PREFIX}/{protocol}"

    def _rewrite_cert_paths(cfg: dict, node_idx: int | None, is_client: bool) -> dict:
        """Replace local cert path values with remote equivalents."""
        for key in ("my_cert_path", "my_cert_key_path", "root_cert_path"):
            if key not in cfg:
                continue
            # Extract just the filename (e.g. "node-0.chain.pem").
            filename = Path(cfg[key]).name
            cfg[key] = f"{remote_dir}/{filename}"
        return cfg

    def _rewrite_net_map(cfg: dict) -> dict:
        """Replace 127.0.0.1 addresses in all *_net_map fields."""
        for map_key in ("net_map", "mempool_net_map"):
            if map_key not in cfg:
                continue
            base = base_port if map_key == "net_map" else mempool_base
            new_map: dict[str, str] = {}
            for node_id_str, addr in cfg[map_key].items():
                node_id = int(node_id_str)
                _, port_str = addr.rsplit(":", 1)
                port = int(port_str)
                ip = node_ip.get(node_id, "127.0.0.1")
                new_map[node_id_str] = f"{ip}:{port}"
            cfg[map_key] = new_map
        if "client_net_map" in cfg:
            # genconfig uses 0-indexed client IDs in client_net_map, but
            # committee.clients uses IDs starting at n (n+0, n+1, ...).
            # Map by sequential position (genconfig_idx → clients[idx]).
            client_list = committee.clients
            new_map = {}
            for genconfig_idx_str, addr in cfg["client_net_map"].items():
                genconfig_idx = int(genconfig_idx_str)
                _, port_str = addr.rsplit(":", 1)
                port = int(port_str)
                if genconfig_idx < len(client_list):
                    ip = client_list[genconfig_idx].endpoint.host
                else:
                    ip = "127.0.0.1"
                new_map[genconfig_idx_str] = f"{ip}:{port}"
            cfg["client_net_map"] = new_map
        return cfg

    # Rewrite nodes-{i}.json
    for i in range(committee.n):
        node_json = out_dir / f"nodes-{i}.json"
        if not node_json.exists():
            continue
        cfg = json.loads(node_json.read_text())
        cfg = _rewrite_net_map(cfg)
        cfg = _rewrite_cert_paths(cfg, node_idx=i, is_client=False)
        node_json.write_text(json.dumps(cfg, indent=2))

    # Rewrite each per-client config (client-0.json, client-1.json, …)
    # plus the legacy `client.json` alias (= client 0) for backward compat.
    client_files = sorted(out_dir.glob("client-*.json")) + [out_dir / "client.json"]
    for client_json in client_files:
        if not client_json.exists():
            continue
        # Identify which committee.clients[] entry this file belongs to.
        # `client-{j}.json` -> j; `client.json` -> 0 (genconfig's alias).
        stem = client_json.stem  # "client-3" or "client"
        if stem == "client":
            client_idx = 0
        else:
            client_idx = int(stem.split("-", 1)[1])
        if client_idx >= len(committee.clients):
            # Shouldn't happen unless genconfig over-mints; skip safely.
            continue
        this_client_ip = committee.clients[client_idx].endpoint.host

        cfg = json.loads(client_json.read_text())
        # net_map in client.json references server IPs (same as nodes).
        if "net_map" in cfg:
            new_map: dict[str, str] = {}
            for node_id_str, addr in cfg["net_map"].items():
                node_id = int(node_id_str)
                _, port_str = addr.rsplit(":", 1)
                port = int(port_str)
                ip = node_ip.get(node_id, "127.0.0.1")
                new_map[node_id_str] = f"{ip}:{port}"
            cfg["net_map"] = new_map
        cfg = _rewrite_cert_paths(cfg, node_idx=None, is_client=True)
        # my_listen_addr serves DOUBLE duty in libapollo-rs's apollo
        # client: it's both the bind address AND the `reply_to`
        # advertised inside every `ClientMsg::NewBatch`, which the
        # server's ConfirmationRouter uses to route per-tx
        # `Confirmation(Hash<Tx>)` back. Setting it to 0.0.0.0 makes
        # the bind succeed but kills the confirmation flow (no
        # routable destination), producing lat=nan. Use the client's
        # actual private IP so it's both bindable on the host (Linux
        # accepts a bind to the host's own IP) and routable from the
        # nodes.
        if "my_listen_addr" in cfg:
            _, port_str = cfg["my_listen_addr"].rsplit(":", 1)
            cfg["my_listen_addr"] = f"{this_client_ip}:{port_str}"
        client_json.write_text(json.dumps(cfg, indent=2))


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

    num_clients = max(1, len(committee.clients))
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
        "-c", str(num_clients),                  # mint N client identities
        "--node_ips", node_ips,
        "--client_ips", client_ips,
        "--payload", "512",                      # match leto/zeus tx_size=512
        "--target", str(out_dir),
    ]
    subprocess.run(cmd, check=True)

    # AWS post-processing: genconfig always writes 127.0.0.1 in all
    # net_map fields and local absolute paths for TLS cert references.
    # Detect AWS mode by checking whether any member uses a non-loopback IP.
    is_aws = any(
        m.endpoint.host not in ("127.0.0.1", "localhost")
        for m in committee.members
    )
    if is_aws:
        _fixup_aws_configs(
            out_dir=out_dir,
            committee=committee,
            base_port=base_port,
            mempool_base=mempool_base,
            client_listen=client_listen,
            protocol=protocol,
        )

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
        "client_config": out_dir / "client.json",       # backward-compat alias = client 0
        "root_cert": out_dir / "root-cert.pem",
    }
    for i in range(committee.n):
        paths[f"node_config_{i}"] = out_dir / f"nodes-{i}.json"
        paths[f"node_chain_{i}"] = out_dir / f"node-{i}.chain.pem"
        paths[f"node_key_{i}"] = out_dir / f"node-{i}.key.pem"
    for j in range(num_clients):
        paths[f"client_config_{j}"] = out_dir / f"client-{j}.json"
        paths[f"client_chain_{j}"] = out_dir / f"client-{j}.chain.pem"
        paths[f"client_key_{j}"] = out_dir / f"client-{j}.key.pem"

    # Verify everything genconfig was supposed to produce actually exists.
    for label, p in paths.items():
        if not p.exists():
            raise RuntimeError(f"genconfig did not produce {label} at {p}")

    return paths
