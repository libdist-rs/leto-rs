"""CLI entrypoint for the leto-rs multi-protocol benchmark orchestrator.

Invoke via `fab <task> [args]` from this directory. Tasks delegate to
modules under `orchestrator/`.

Every task accepts an optional `--config <file.toml>`. When supplied,
the task reads its defaults from the file; CLI flags override.

Run a whole experiment in declared order:

    fab run --config experiments/pareto.toml
"""

from __future__ import annotations

from pathlib import Path

from fabric import task

from orchestrator import config as cfg_lib


# ---------------------------------------------------------------------------
# Per-task config helpers
# ---------------------------------------------------------------------------


def _load_cfg(config_path: str | None) -> dict:
    """Return the parsed TOML config, or an empty dict when no path given."""
    return cfg_lib.load(config_path) if config_path else {}


def _to_bool(val):
    """Normalise fabric's string args + TOML bools into Python bools."""
    if isinstance(val, bool):
        return val
    return str(val).lower() in ("1", "true", "yes")


def _to_int(val):
    return int(val) if val is not None else None


def _expand(path):
    if path is None:
        return None
    return str(Path(str(path)).expanduser())


# ---------------------------------------------------------------------------
# Tasks
# ---------------------------------------------------------------------------


@task
def provision(c, config=None,
              num_nodes=None, num_clients=None, instance_type=None,
              az=None, spot=None, key_name=None,
              security_group_id=None, subnet_id=None, tag=None,
              root_volume_gb=None):
    """Provision EC2 instances on AWS.

    Defaults (when neither CLI nor config supplies a value, falls back
    to orchestrator/aws.py): c8g.xlarge on-demand in us-west-2d with a
    30 GB gp3 root volume.

    Requires AWS credentials in env or ~/.aws/. Caller supplies an
    existing key pair, security group, and subnet ID in the target AZ.
    """
    cfg = _load_cfg(config)
    from orchestrator import aws

    kwargs: dict = {}
    num_nodes = cfg_lib.cli_or(num_nodes, cfg, "aws", "num_nodes", default=4)
    num_clients = cfg_lib.cli_or(num_clients, cfg, "aws", "num_clients", default=2)
    kwargs["num_nodes"] = int(num_nodes)
    kwargs["num_clients"] = int(num_clients)

    az_val = cfg_lib.cli_or(az, cfg, "aws", "az", default=None)
    if az_val is not None:
        kwargs["az"] = az_val

    for k, cli_v in (
        ("key_name", key_name),
        ("security_group_id", security_group_id),
        ("subnet_id", subnet_id),
        ("tag", tag),
        ("instance_type", instance_type),
    ):
        v = cfg_lib.cli_or(cli_v, cfg, "aws", k, default=None)
        if v is not None:
            kwargs[k] = v

    spot_v = cfg_lib.cli_or(spot, cfg, "aws", "spot", default=None)
    if spot_v is not None:
        kwargs["spot"] = _to_bool(spot_v)

    rvg = cfg_lib.cli_or(root_volume_gb, cfg, "aws", "root_volume_gb", default=None)
    if rvg is not None:
        kwargs["root_volume_gb"] = int(rvg)

    instances = aws.provision(**kwargs)
    print(f"provisioned {len(instances)} instance(s)")
    aws.status()


@task
def install(c, config=None, target="aws", ssh_key_path=None, protocols=None):
    """Bootstrap + clone protocol repos at pinned SHAs on each host.

    Local mode: clones into scripts/state/repos/ (not yet wired — use
    `fab smoke --target local --protocol leto` which uses the parent
    leto-rs checkout directly).

    AWS mode: requires `ssh_key_path` (via --ssh-key-path or
    `[aws].ssh_key_path` in the config). Idempotent.
    """
    cfg = _load_cfg(config)
    from orchestrator import deploy
    from orchestrator.protocols import REGISTRY

    protos = cfg_lib.cli_or(
        protocols, cfg, "install", "protocols",
        default="apollo,artemis,leto,zeus,mysticeti",
    )
    proto_list = [REGISTRY[p] for p in cfg_lib.as_list(protos)]

    if target == "local":
        print("local install: cloning into scripts/state/repos/ is pending; "
              "for now use --target=local smoke which builds in-place")
        return

    ssh = _expand(cfg_lib.cli_or(ssh_key_path, cfg, "aws", "ssh_key_path"))
    if not ssh:
        raise SystemExit(
            "remote install requires ssh_key_path (CLI or [aws].ssh_key_path)"
        )
    from orchestrator import aws
    state = aws.load_state()
    deploy.install_remote(state, proto_list, ssh)
    print(f"install complete on {len(state['instances'])} hosts")


@task
def build(c, config=None, target="aws", ssh_key_path=None, protocols=None):
    """Build each protocol's binaries on every host.

    Local mode: cargo build --release --all in the leto-rs workspace.
    AWS mode: build on node 0 + distribute binaries via tarball + scp.
    """
    cfg = _load_cfg(config)
    if target == "local":
        import subprocess
        subprocess.run(["cargo", "build", "--release", "--all"], check=True)
        print("local build complete")
        return

    from orchestrator import deploy, aws
    from orchestrator.protocols import REGISTRY

    protos = cfg_lib.cli_or(
        protocols, cfg, "build", "protocols",
        default=cfg_lib.lookup(
            cfg, "install", "protocols",
            default="apollo,artemis,leto,zeus,mysticeti",
        ),
    )
    proto_list = [REGISTRY[p] for p in cfg_lib.as_list(protos)]

    ssh = _expand(cfg_lib.cli_or(ssh_key_path, cfg, "aws", "ssh_key_path"))
    if not ssh:
        raise SystemExit(
            "remote build requires ssh_key_path (CLI or [aws].ssh_key_path)"
        )
    state = aws.load_state()
    deploy.build_remote(state, proto_list, ssh)
    print(f"build complete on {len(state['instances'])} hosts")


@task
def smoke(c, config=None, target="local", protocol=None,
          num_nodes=None, num_clients=None,
          rate=None, duration=None, ssh_key_path=None):
    """Quick end-to-end run for one protocol — wiring validation.

    AWS mode requires `ssh_key_path` (CLI or [aws].ssh_key_path).
    """
    cfg = _load_cfg(config)
    from orchestrator.bench import SweepConfig, run_sweep

    protocol = cfg_lib.cli_or(protocol, cfg, "smoke", "protocol", default="leto")
    num_nodes = int(cfg_lib.cli_or(num_nodes, cfg, "aws", "num_nodes", default=4))
    num_clients = int(cfg_lib.cli_or(num_clients, cfg, "aws", "num_clients", default=2))
    rate = int(cfg_lib.cli_or(rate, cfg, "smoke", "rate", default=5000))
    duration = int(cfg_lib.cli_or(duration, cfg, "smoke", "duration", default=30))
    ssh = _expand(cfg_lib.cli_or(ssh_key_path, cfg, "aws", "ssh_key_path"))

    sweep_cfg = SweepConfig(
        protocols=[protocol],
        t_values=[(num_nodes - 1) // 3],
        loads=[rate],
        trials=1,
        warmup_secs=5,
        measure_secs=duration,
        target=target,
        tag=f"smoke-{protocol}",
        ssh_key_path=ssh,
    )
    out = run_sweep(sweep_cfg)
    print(f"smoke results: {out}")


@task
def bench(c, config=None,
          runs=None, faults_t=None, protocols=None,
          loads=None, load_mode="ramp", faults="none", tag=None,
          target="aws", ssh_key_path=None,
          warmup_secs=None, measure_secs=None):
    """Run a sweep matrix: protocols × scales × loads × trials.

    AWS mode requires `ssh_key_path` (CLI or [aws].ssh_key_path).
    """
    cfg = _load_cfg(config)
    from orchestrator.bench import SweepConfig, run_sweep

    runs_v = int(cfg_lib.cli_or(runs, cfg, "bench", "runs", default=3))

    t_raw = cfg_lib.cli_or(faults_t, cfg, "bench", "t_values", default=[1])
    if isinstance(t_raw, list):
        t_values = [int(x) for x in t_raw]
    else:
        t_values = [int(x) for x in str(t_raw).split(",") if x]

    protos = cfg_lib.cli_or(
        protocols, cfg, "bench", "protocols",
        default="apollo,artemis,leto,zeus,mysticeti",
    )
    proto_list = cfg_lib.as_list(protos)

    loads_raw = cfg_lib.cli_or(
        loads, cfg, "bench", "loads",
        default=[1000, 2500, 5000, 10000, 25000, 50000, 100000, 200000],
    )
    if isinstance(loads_raw, list):
        loads_list = [int(x) for x in loads_raw]
    else:
        loads_list = [
            int(float(x.replace("k", "")) * 1000)
            for x in str(loads_raw).split(",") if x
        ]

    tag_v = cfg_lib.cli_or(tag, cfg, "meta", "tag", default="untagged")
    warm_v = int(cfg_lib.cli_or(warmup_secs, cfg, "bench", "warmup_secs", default=60))
    meas_v = int(cfg_lib.cli_or(measure_secs, cfg, "bench", "measure_secs", default=120))
    ssh = _expand(cfg_lib.cli_or(ssh_key_path, cfg, "aws", "ssh_key_path"))

    sweep_cfg = SweepConfig(
        protocols=proto_list,
        t_values=t_values,
        loads=loads_list,
        trials=runs_v,
        warmup_secs=warm_v,
        measure_secs=meas_v,
        target=target,
        tag=tag_v,
        ssh_key_path=ssh,
    )
    out = run_sweep(sweep_cfg)
    print(f"sweep results: {out}")


@task
def scaling(c, config=None, runs=None, protocols=None,
            loads=None, tag=None,
            warmup_secs=None, measure_secs=None, ssh_key_path=None,
            no_scale_down=False):
    """Reverse-scaling sweep: run bench for the largest t first, then
    terminate excess hosts before each smaller t.

    Assumes the cluster is already provisioned + installed + built for
    the LARGEST t value (largest cluster).  Each phase produces its own
    timestamped results dir keyed by t (`<tag>-t<N>-<stamp>`); plot can
    aggregate across them.

    Pass `--no-scale-down` to keep the full cluster up between phases
    (useful for debugging).
    """
    cfg = _load_cfg(config)
    from orchestrator import aws as aws_mod
    from orchestrator.bench import SweepConfig, run_sweep

    t_raw = cfg_lib.lookup(cfg, "bench", "t_values", default=[1])
    t_values = [int(x) for x in t_raw] if isinstance(t_raw, list) else \
               [int(x) for x in str(t_raw).split(",") if x]
    t_sorted = sorted(t_values, reverse=True)

    runs_v = int(cfg_lib.cli_or(runs, cfg, "bench", "runs", default=3))

    loads_raw = cfg_lib.cli_or(
        loads, cfg, "bench", "loads", default=[50000],
    )
    if isinstance(loads_raw, list):
        loads_list = [int(x) for x in loads_raw]
    else:
        loads_list = [
            int(float(x.replace("k", "")) * 1000)
            for x in str(loads_raw).split(",") if x
        ]

    protos = cfg_lib.cli_or(
        protocols, cfg, "bench", "protocols",
        default="apollo,artemis,leto,zeus,mysticeti",
    )
    proto_list = cfg_lib.as_list(protos)

    tag_base = cfg_lib.cli_or(tag, cfg, "meta", "tag", default="scaling")
    warm_v = int(cfg_lib.cli_or(warmup_secs, cfg, "bench", "warmup_secs", default=10))
    meas_v = int(cfg_lib.cli_or(measure_secs, cfg, "bench", "measure_secs", default=110))
    ssh = _expand(cfg_lib.cli_or(ssh_key_path, cfg, "aws", "ssh_key_path"))

    skip_scale_down = _to_bool(no_scale_down)

    for idx, t in enumerate(t_sorted):
        n = 3 * t + 1
        nc = max(1, (n + 2) // 3)
        print(f"\n=== SCALING PHASE: t={t}  n={n}  clients={nc} ===")

        sweep_cfg = SweepConfig(
            protocols=proto_list,
            t_values=[t],
            loads=loads_list,
            trials=runs_v,
            warmup_secs=warm_v,
            measure_secs=meas_v,
            target="aws",
            tag=f"{tag_base}-t{t}",
            ssh_key_path=ssh,
        )
        run_sweep(sweep_cfg)

        # Scale down to the next-smaller t's footprint, unless this is
        # the last phase.
        if idx + 1 >= len(t_sorted):
            continue
        if skip_scale_down:
            print("  --no-scale-down: leaving cluster intact")
            continue
        t_next = t_sorted[idx + 1]
        n_next = 3 * t_next + 1
        nc_next = max(1, (n_next + 2) // 3)
        print(f"\n  scaling cluster down: t={t}->{t_next} "
              f"({n}n+{nc}c -> {n_next}n+{nc_next}c)")
        aws_mod.scale_down(target_nodes=n_next, target_clients=nc_next)


@task
def plot(c, config=None, tag=None, x_axis=None):
    """Render figures from a tagged run's results.jsonl."""
    cfg = _load_cfg(config)
    from orchestrator.plot import render_pareto, render_scaling, render_crash_table
    from orchestrator.bench import state_root

    tag_v = cfg_lib.cli_or(tag, cfg, "meta", "tag", default="latest")
    x_v = cfg_lib.cli_or(x_axis, cfg, "plot", "x_axis", default="throughput")

    results_root = state_root() / "results"
    candidates = sorted(
        p for p in results_root.iterdir()
        if p.is_dir() and p.name.startswith(tag_v)
    )
    if not candidates:
        raise SystemExit(f"no results found under {results_root} matching {tag_v!r}")
    target = candidates[-1]
    jsonl = target / "results.jsonl"
    plots_dir = target / "plots"
    if x_v == "throughput":
        render_pareto(jsonl, plots_dir / "pareto.png")
    elif x_v == "n":
        render_scaling(jsonl, plots_dir / "scaling.png")
    elif x_v == "crash":
        render_crash_table(jsonl, plots_dir / "crash.csv")
    else:
        raise SystemExit(f"unknown x_axis {x_v!r}; use throughput|n|crash")
    print(f"plot written under {plots_dir}/")


@task
def status(c, config=None):
    """List provisioned instances + estimated hourly cost."""
    from orchestrator import aws
    aws.status()


@task
def destroy(c, config=None, target="aws"):
    """Terminate all provisioned AWS instances. Idempotent."""
    if target == "local":
        from orchestrator.deploy import kill_session
        kill_session()
        print("local tmux session killed")
        return
    from orchestrator import aws
    aws.destroy()


# ---------------------------------------------------------------------------
# fab run — execute meta.steps in declared order
# ---------------------------------------------------------------------------


# Map step name → (callable, kwargs builder). Kept here so the
# orchestration logic is one short table instead of a long if/elif chain.
_STEP_DISPATCH = {
    "provision": provision,
    "install":   install,
    "build":     build,
    "smoke":     smoke,
    "bench":     bench,
    "scaling":   scaling,
    "plot":      plot,
    "destroy":   destroy,
}


@task
def run(c, config):
    """Execute every step listed in [meta].steps of the given config,
    in declared order.  Each step reads its own section + [aws] from the
    same file.  Aborts on the first step that raises.
    """
    parsed = cfg_lib.load(config)
    steps = cfg_lib.lookup(parsed, "meta", "steps", default=[])
    if not steps:
        raise SystemExit(f"no [meta].steps in {config}")
    print(f"[run] config={config} steps={steps}")
    for step in steps:
        fn = _STEP_DISPATCH.get(step)
        if fn is None:
            raise SystemExit(f"unknown step {step!r}; valid: {list(_STEP_DISPATCH)}")
        print(f"\n=== STEP: {step} ===")
        fn(c, config=config)
    print("\n[run] all steps completed")
