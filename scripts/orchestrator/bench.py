"""Sweep runner.

For the lean+ paper:
- Pareto: per-protocol × n=4 × ~10 load points × 3 trials.
- Scalability: per-protocol × t ∈ {1,2,5,10,20} × 1 saturating load × 3 trials.
- Crash-fault: per-protocol × n=4 × 1 fault × 3 trials.

A sweep iterates (protocol, t, load, trial), constructs the canonical
committee, generates per-protocol configs via translators, launches via
deploy, waits the measurement window, kills the session, parses logs,
and appends rows to results.jsonl.
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass, asdict
from datetime import datetime
from itertools import product
from pathlib import Path
from typing import Optional

from orchestrator import deploy, genconfig, parse
from orchestrator.protocols import REGISTRY, Protocol


@dataclass
class SweepConfig:
    protocols: list[str]
    t_values: list[int]           # tolerated faults; n = 3t+1
    loads: list[int]              # offered tx/s per data point
    trials: int = 3
    warmup_secs: int = 5
    measure_secs: int = 30
    target: str = "local"         # "local" or "aws"
    tag: str = "untagged"
    # Path to the SSH private key used by fabric for AWS hosts.
    # Required when target == "aws". Ignored otherwise.
    ssh_key_path: str | None = None

    def num_clients_for(self, n: int) -> int:
        # ⌈n/3⌉ matches the plan; minimum 1.
        return max(1, (n + 2) // 3)


def state_root() -> Path:
    return Path(__file__).resolve().parent.parent / "state"


def run_sweep(cfg: SweepConfig, total_txs: int = 0, window: int = 0) -> Path:
    """Run the sweep, return the results.jsonl path."""
    stamp = f"{cfg.tag}-{datetime.utcnow().strftime('%Y%m%dT%H%M%SZ')}"
    out_root = state_root() / "results" / stamp
    out_root.mkdir(parents=True, exist_ok=True)

    manifest: dict = {
        "tag": cfg.tag,
        "stamp": stamp,
        "config": asdict(cfg),
        "runs": [],
    }
    samples: list[parse.Sample] = []

    for protocol_name, t, load, trial in product(
        cfg.protocols, cfg.t_values, cfg.loads, range(cfg.trials)
    ):
        protocol = REGISTRY[protocol_name]
        n = 3 * t + 1
        num_clients = cfg.num_clients_for(n)
        run_id = f"{protocol_name}-n{n}-f{t}/run-{trial}"
        run_dir = out_root / f"{protocol_name}-n{n}-f{t}" / f"run-{trial}"
        config_dir = state_root() / "configs" / stamp / f"{protocol_name}-n{n}-f{t}-trial{trial}"

        committee = genconfig.generate_local(n=n, f=t, num_clients=num_clients)
        genconfig.write_committee(committee, config_dir)

        translator = _import_translator(protocol.translator_module)
        translator.translate(committee, config_dir, protocol=protocol_name)

        # Default driver volumes if not supplied.
        # total_txs sized so the closed-loop client (apollo/artemis) keeps
        # firing for the whole measurement window; ignored by open-loop
        # clients (leto/zeus use burst_interval_ms instead).
        effective_total_txs = total_txs or max(100_000, load * cfg.measure_secs * 2)
        # window must be large enough to keep apollo/artemis's pipeline
        # full; too-small windows starve the closed-loop into idle. Empirical
        # smoke shows 5_000 works for apollo-class protocols at n=4.
        effective_window = window or max(5_000, load // 10)

        if cfg.target == "local":
            deploy.kill_session()
            deploy.launch_local(
                protocol=protocol,
                config_dir=config_dir,
                n=n,
                num_clients=num_clients,
                log_dir=run_dir,
                rate=load,
                total_txs=effective_total_txs,
                window=effective_window,
            )
            # launch_local is non-blocking — sleep for the run window
            # here, then kill the session.
            time.sleep(cfg.warmup_secs + cfg.measure_secs)
            deploy.kill_session()
        else:
            if not cfg.ssh_key_path:
                raise ValueError(
                    "target='aws' requires ssh_key_path in SweepConfig"
                )
            # Remote launch is blocking: it sleeps warmup+measure inside
            # launch_remote and tears down + fetches logs before
            # returning.  No extra sleep needed in the caller.
            from orchestrator import aws as _aws
            state = _aws.load_state()
            deploy.launch_remote(
                state=state,
                protocol=protocol,
                config_dir=config_dir,
                n=n,
                num_clients=num_clients,
                log_dir=run_dir,
                rate=load,
                total_txs=effective_total_txs,
                window=effective_window,
                ssh_key_path=cfg.ssh_key_path,
                warmup_secs=cfg.warmup_secs,
                measure_secs=cfg.measure_secs,
            )

        manifest["runs"].append({
            "protocol": protocol_name,
            "n": n,
            "f": t,
            "trial": trial,
            "rate_target": load,
            "run_id": run_id,
        })
        sample = parse.parse_run_dir(
            run_dir,
            protocol=protocol_name,
            n=n,
            f=t,
            trial=trial,
            rate_target=load,
            run_id=run_id,
        )
        if sample is not None:
            samples.append(sample)

    (out_root / "manifest.json").write_text(json.dumps(manifest, indent=2))
    out_jsonl = out_root / "results.jsonl"
    count = parse.write_jsonl(samples, out_jsonl)
    print(f"sweep complete: {count} samples → {out_jsonl}")
    return out_jsonl


def _import_translator(module_path: str):
    """Lazy dotted-path import — avoids loading every translator at module
    load time."""
    if not module_path:
        raise ValueError("translator_module unset on Protocol")
    import importlib
    return importlib.import_module(module_path)
