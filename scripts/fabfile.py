"""CLI entrypoint for the leto-rs multi-protocol benchmark orchestrator.

Invoke via `fab <task> [args]` from this directory. Tasks delegate to
modules under `orchestrator/`.
"""

from __future__ import annotations

from pathlib import Path

from fabric import task


@task
def provision(c, num_nodes=4, num_clients=2, instance_type="c8g.large",
              az="us-west-2d", spot=True, key_name=None,
              security_group_id=None, subnet_id=None, tag="leto-bench"):
    """Provision EC2 instances on AWS (spot c8g.large in us-west-2d default).

    Requires AWS credentials in env or ~/.aws/. Caller supplies an
    existing key pair, security group, and subnet ID in the target AZ.
    """
    from orchestrator import aws
    instances = aws.provision(
        num_nodes=int(num_nodes),
        num_clients=int(num_clients),
        instance_type=instance_type,
        az=az,
        spot=bool(spot),
        key_name=key_name,
        security_group_id=security_group_id,
        subnet_id=subnet_id,
        tag=tag,
    )
    print(f"provisioned {len(instances)} instance(s)")
    aws.status()


@task
def install(c, target="aws"):
    """Clone protocol repos at pinned SHAs into state/repos/ on each host."""
    if target == "local":
        from orchestrator.deploy import install_remote  # placeholder
        print("local install: clone repos into scripts/state/repos/")
        return
    raise NotImplementedError(
        "remote install pending — see orchestrator/deploy.py::install_remote"
    )


@task
def build(c, target="aws"):
    """Build each protocol's binaries on each host (remote) or locally."""
    if target == "local":
        import subprocess
        # Just exercise the leto/zeus side of the workspace; apollo+mysticeti
        # cloned-checkout builds plug in once state/repos/ is populated.
        subprocess.run(["cargo", "build", "--release", "--all"], check=True)
        print("local build complete")
        return
    raise NotImplementedError("remote build pending")


@task
def smoke(c, target="local", protocol="leto", num_nodes=4, num_clients=2,
          rate=5000, duration=30):
    """Quick end-to-end run for one protocol — wiring validation."""
    from orchestrator.bench import SweepConfig, run_sweep
    cfg = SweepConfig(
        protocols=[protocol],
        t_values=[(int(num_nodes) - 1) // 3],
        loads=[int(rate)],
        trials=1,
        warmup_secs=5,
        measure_secs=int(duration),
        target=target,
        tag=f"smoke-{protocol}",
    )
    out = run_sweep(cfg)
    print(f"smoke results: {out}")


@task
def bench(c, runs=3, t="1", protocols="apollo,artemis,leto,zeus,mysticeti",
          loads=None, load_mode="ramp", faults="none", tag="untagged",
          target="aws"):
    """Run a sweep matrix: protocols × scales × loads × trials."""
    from orchestrator.bench import SweepConfig, run_sweep
    t_values = [int(x) for x in str(t).split(",") if x]
    if loads is None:
        # Default ramp; adjust per --load-mode in a future iteration.
        loads_list = [1000, 2500, 5000, 10000, 25000, 50000, 100000, 200000]
    else:
        loads_list = [int(x.replace("k", "000")) for x in str(loads).split(",") if x]
    cfg = SweepConfig(
        protocols=[p.strip() for p in protocols.split(",") if p.strip()],
        t_values=t_values,
        loads=loads_list,
        trials=int(runs),
        target=target,
        tag=tag,
    )
    out = run_sweep(cfg)
    print(f"sweep results: {out}")


@task
def plot(c, tag="latest", x_axis="throughput"):
    """Render figures from a tagged run's results.jsonl."""
    from orchestrator.plot import render_pareto, render_scaling, render_crash_table
    from orchestrator.bench import state_root
    results_root = state_root() / "results"
    # Find the latest dir matching tag prefix.
    candidates = sorted(p for p in results_root.iterdir() if p.is_dir() and p.name.startswith(tag))
    if not candidates:
        raise SystemExit(f"no results found under {results_root} matching {tag!r}")
    target = candidates[-1]
    jsonl = target / "results.jsonl"
    plots_dir = target / "plots"
    if x_axis == "throughput":
        render_pareto(jsonl, plots_dir / "pareto.png")
    elif x_axis == "n":
        render_scaling(jsonl, plots_dir / "scaling.png")
    elif x_axis == "crash":
        render_crash_table(jsonl, plots_dir / "crash.csv")
    else:
        raise SystemExit(f"unknown x_axis {x_axis!r}; use throughput|n|crash")
    print(f"plot written under {plots_dir}/")


@task
def status(c):
    """List provisioned instances + estimated hourly cost."""
    from orchestrator import aws
    aws.status()


@task
def destroy(c, target="aws"):
    """Terminate all provisioned AWS instances. Idempotent."""
    if target == "local":
        from orchestrator.deploy import kill_session
        kill_session()
        print("local tmux session killed")
        return
    from orchestrator import aws
    aws.destroy()
