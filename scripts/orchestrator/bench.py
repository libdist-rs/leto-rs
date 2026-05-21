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
    # Open results.jsonl up front and append per-run so a long sweep is
    # tailable in real time. Final `parse.write_jsonl` at the end is a
    # full rewrite for consistency (dedups partial-write rows on rerun).
    out_jsonl = out_root / "results.jsonl"
    out_jsonl.touch()

    # For AWS runs, load the instance list once so every iteration can
    # build a real-IP committee without re-reading state.json each time.
    _aws_state: dict | None = None
    if cfg.target == "aws":
        from orchestrator import aws as _aws_mod
        _aws_state = _aws_mod.load_state()

    # Iteration order: trial is OUTERMOST so the sweep covers every
    # (protocol, t, load) cell once before repeating any of them. After
    # one full pass through trial=0 you can tail results.jsonl and
    # already see one sample per cell — enough to spot a broken protocol
    # or a wrong-knee load curve hours before the run finishes, instead
    # of staring at "still inside apollo" for the first third of the
    # sweep. Each trial pass is also a clean checkpoint: if a sweep is
    # aborted halfway through trial=2, you still have 2 complete passes
    # in the results dir.
    #
    # Cache (protocol, t) cells that have already had their configs
    # pushed to every host — a *set*, not just the last one, because
    # outer-loop trial means we revisit each (protocol, t) once per
    # trial, and after the first trial pass every cell is cached for
    # the rest of the sweep. Skipping the redundant per-host SCP saves
    # ~5-10 s × (#cells × (trials-1)).
    _pushed_committees: set[tuple[str, int]] = set()

    sweep_iter = list(product(cfg.protocols, cfg.t_values, cfg.loads))
    for trial in range(cfg.trials):
        print(f"\n=== TRIAL {trial + 1}/{cfg.trials} ({len(sweep_iter)} cells) ===")
        for protocol_name, t, load in sweep_iter:
            protocol = REGISTRY[protocol_name]
            n = 3 * t + 1
            num_clients = cfg.num_clients_for(n)
            run_id = f"{protocol_name}-n{n}-f{t}-r{load}/run-{trial}"
            run_dir = out_root / f"{protocol_name}-n{n}-f{t}" / f"load-{load}" / f"run-{trial}"
            # config_dir is keyed on (protocol, t) only — the produced
            # config files do not depend on load or trial, so the same
            # dir is reused across every iteration of a (protocol, t)
            # cell and launch_remote can skip its SCP push after the
            # first time we visit each cell.
            config_dir = state_root() / "configs" / stamp / f"{protocol_name}-n{n}-f{t}"
            committee_key: tuple[str, int] = (protocol_name, t)
            configs_already_generated = committee_key in _pushed_committees

            if cfg.target == "aws" and _aws_state is not None:
                # Build a committee using the actual private IPs of provisioned
                # instances — nodes first (ordered by aws.json), then clients.
                instances = _aws_state.get("instances", [])
                node_hosts = [i["private_ip"] for i in instances if i.get("role") == "node"][:n]
                client_hosts = [i["private_ip"] for i in instances if i.get("role") == "client"][:num_clients]
                if len(node_hosts) < n or len(client_hosts) < num_clients:
                    raise RuntimeError(
                        f"need {n} node hosts + {num_clients} client hosts; "
                        f"provisioned {len(node_hosts)} nodes + {len(client_hosts)} clients"
                    )
                committee = genconfig.generate_aws(
                    node_hosts=node_hosts,
                    client_hosts=client_hosts,
                    f=t,
                )
            else:
                committee = genconfig.generate_local(n=n, f=t, num_clients=num_clients)

            if not configs_already_generated:
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

            # Wrap each run in try/except so a transient failure (laptop
            # network flip, mid-run SSH RST, ephemeral AWS hiccup) costs
            # exactly one row instead of killing the whole 4-hour sweep.
            # Local-mode failures are also captured; the cost of a missed
            # row is the same in both targets.
            try:
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
                        skip_config_push=configs_already_generated,
                    )
            except Exception as e:
                print(
                    f"  ! {protocol_name} n={n} f={t} load={load} trial={trial}: "
                    f"LAUNCH FAILED — {type(e).__name__}: {e}"
                )
                with (out_root / "errors.log").open("a") as ef:
                    ef.write(
                        f"{datetime.utcnow().isoformat()} {run_id} "
                        f"{type(e).__name__}: {e}\n"
                    )
                # The next run's launch_remote starts with `tmux kill-session`
                # on every host (deploy.py:540), so leftover processes from a
                # crashed run get cleaned up automatically. Carry on.
                #
                # Evict the cell from the push cache: the failed launch
                # may have left half-distributed configs on remote, so
                # the next visit (next trial, or same trial if scaling)
                # must re-push to recover.
                _pushed_committees.discard(committee_key)
                continue

            # Launch succeeded: any later visit to this (protocol, t)
            # cell — whether on the same trial pass or a later one —
            # can skip the per-host config push.
            _pushed_committees.add(committee_key)

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
                with out_jsonl.open("a") as f:
                    f.write(json.dumps(asdict(sample)) + "\n")
                print(
                    f"  → {protocol_name} n={n} f={t} load={load} trial={trial}: "
                    f"thr={sample.throughput:.0f} tx/s  lat={sample.latency_ms:.1f} ms"
                )
            else:
                print(
                    f"  → {protocol_name} n={n} f={t} load={load} trial={trial}: NO DP[] PARSED"
                )

    (out_root / "manifest.json").write_text(json.dumps(manifest, indent=2))
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
