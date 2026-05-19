"""Deployment — local tmux + remote SSH.

Local mode: spawns N nodes + M clients in tmux windows on this host.
Remote mode: uses fabric.Connection.run over SSH to start binaries on
provisioned AWS hosts.

The two modes share the per-protocol command construction so behaviour
is identical at the binary-invocation level — only the launcher differs.
"""

from __future__ import annotations

import shlex
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from orchestrator.protocols import Protocol


# ---------------------------------------------------------------------------
# Local tmux launcher
# ---------------------------------------------------------------------------


@dataclass
class LocalProcess:
    """One launched process under the local tmux session."""

    tmux_window: str
    log_path: Path
    pid_file: Optional[Path] = None


def _tmux(args: list[str]) -> None:
    """Best-effort tmux invocation."""
    subprocess.run(["tmux", *args], check=False)


def kill_session(session: str = "leto-bench") -> None:
    """Tear down any prior tmux session with the same name.

    Also pkills lingering `node` / `node-zeus` processes so port binds
    succeed on the next launch (the tmux window kill doesn't propagate
    to child processes reliably on macOS).
    """
    _tmux(["kill-session", "-t", session])
    for name in ("node", "node-zeus"):
        subprocess.run(["pkill", "-f", f"target/release/{name} "], check=False)
    time.sleep(0.5)


def launch_local(
    protocol: Protocol,
    config_dir: Path,
    n: int,
    num_clients: int,
    log_dir: Path,
    rate: int,
    total_txs: int,
    window: int,
    session: str = "leto-bench",
    workspace_root: Optional[Path] = None,
) -> list[LocalProcess]:
    """Spawn `n` nodes + `num_clients` clients in tmux windows.

    Returns one LocalProcess per spawned binary. Caller is responsible
    for `kill_session` after the measurement window elapses.

    Assumes `cargo build --release` has already been run; binaries live
    at `<workspace_root>/<protocol.bin_dir>/`.
    """
    log_dir.mkdir(parents=True, exist_ok=True)
    kill_session(session)
    _tmux(["new-session", "-d", "-s", session, "-n", "warmup"])

    if workspace_root is None:
        # For leto/zeus the workspace IS this repo; for others assume
        # state/repos/<name>/ checkout.
        if protocol.name in ("leto", "zeus"):
            workspace_root = Path(__file__).resolve().parent.parent.parent
        else:
            workspace_root = Path(__file__).resolve().parent.parent / "state" / "repos" / protocol.name
    bin_dir = workspace_root / protocol.bin_dir

    processes: list[LocalProcess] = []

    # Server config file is shared across nodes; the binary picks its
    # role via --id.
    server_config = config_dir / f"{protocol.name}-server.json"
    if not server_config.exists():
        raise FileNotFoundError(f"missing server config: {server_config}")

    # Key files: for leto/zeus we either symlink from the repo's
    # examples/ or generate via `cargo r -p node -- keys -n <N>` once
    # per config dir. Use existing examples/ if available; flag missing.
    keys_dir = workspace_root / "examples"

    for node_id in range(n):
        log_path = log_dir / f"node-{node_id}.log"
        key_file = keys_dir / f"keys-{node_id}.json"
        if not key_file.exists():
            raise FileNotFoundError(
                f"missing key file {key_file}; run "
                f"`cargo r -p node -- keys -n {n} -o {keys_dir}` first"
            )
        cmd = protocol.node_run_cmd.format(
            bin_dir=bin_dir,
            config=server_config,
            id=node_id,
            key_file=key_file,
            extra=protocol.node_extra_args,
        )
        # Redirect both stdout and stderr to the per-node log
        # (DP[...] is on stderr; combine for ease of parsing).
        full = f"{cmd} > {shlex.quote(str(log_path))} 2>&1"
        window_name = f"node-{node_id}"
        _tmux(["new-window", "-t", session, "-n", window_name])
        _tmux(["send-keys", "-t", f"{session}:{window_name}", full, "C-m"])
        processes.append(LocalProcess(tmux_window=window_name, log_path=log_path))

    # Give nodes a moment to bind ports before clients connect.
    time.sleep(2)

    for cli_idx in range(num_clients):
        client_id = n + cli_idx
        client_config = config_dir / f"{protocol.name}-client-{client_id}.json"
        if not client_config.exists():
            raise FileNotFoundError(f"missing client config: {client_config}")
        log_path = log_dir / f"client-{client_id}.log"
        cmd = protocol.client_run_cmd.format(
            bin_dir=bin_dir,
            config=client_config,
            id=client_id,
            rate=rate,
            total_txs=total_txs,
            window=window,
            extra=protocol.client_extra_args,
        )
        full = f"{cmd} > {shlex.quote(str(log_path))} 2>&1"
        window_name = f"client-{client_id}"
        _tmux(["new-window", "-t", session, "-n", window_name])
        _tmux(["send-keys", "-t", f"{session}:{window_name}", full, "C-m"])
        processes.append(LocalProcess(tmux_window=window_name, log_path=log_path))

    # Optional sidecar (Mysticeti's dpbridge).
    if protocol.sidecar_run_cmd:
        bridges_dir = Path(__file__).resolve().parent.parent / "bridges"
        log_path = log_dir / "sidecar.log"
        for node_id in range(min(1, n)):  # one sidecar per metrics node
            metrics_url = "http://127.0.0.1:1500/metrics"   # PINME per protocol
            cmd = protocol.sidecar_run_cmd.format(
                bridges_dir=bridges_dir,
                metrics_url=metrics_url,
                extra="",
            )
            full = f"{cmd} > {shlex.quote(str(log_path))} 2>&1"
            window_name = f"sidecar-{node_id}"
            _tmux(["new-window", "-t", session, "-n", window_name])
            _tmux(["send-keys", "-t", f"{session}:{window_name}", full, "C-m"])
            processes.append(LocalProcess(tmux_window=window_name, log_path=log_path))

    return processes


# ---------------------------------------------------------------------------
# Remote SSH launcher
# ---------------------------------------------------------------------------


def install_remote(*args, **kwargs):
    """Clone protocol repos at pinned SHAs on all hosts. PINME."""
    raise NotImplementedError(
        "remote install pending; use local mode for smoke. Implement via "
        "fabric.SerialGroup with `git clone --depth=1 && git checkout <sha>` "
        "and a build step."
    )


def build_remote(*args, **kwargs):
    """Build protocol binaries on remote hosts. PINME."""
    raise NotImplementedError(
        "remote build pending; use local mode for smoke. Implement via "
        "fabric.SerialGroup with `cargo build --release` per protocol."
    )


def launch_remote(*args, **kwargs):
    """Spawn nodes + clients on remote AWS hosts via SSH. PINME."""
    raise NotImplementedError(
        "remote launch pending; use local mode for smoke. Implement via "
        "fabric.Connection per host with the same node_run_cmd / "
        "client_run_cmd templates as launch_local."
    )
