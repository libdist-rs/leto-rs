# leto-rs benchmark orchestrator

Multi-protocol BFT-SMR benchmark harness for the Leto/Zeus paper.
Drives Apollo, Artemis, Leto, Zeus, and Mysticeti from one entrypoint,
on local tmux or AWS spot instances, with a unified `DP[…]` log format.

## Layout

```
scripts/
├── fabfile.py                 # CLI entrypoint (fab provision/install/build/bench/plot/destroy)
├── orchestrator/              # Python package
│   ├── protocols.py           # Protocol metadata (git url, SHA pin, build/run cmds)
│   ├── genconfig.py           # Canonical committee JSON generator
│   ├── translators/           # per-protocol config translators
│   ├── aws.py                 # EC2 provisioning (boto3)
│   ├── deploy.py              # local tmux + remote ssh deploy
│   ├── bench.py               # sweep runner
│   ├── parse.py               # DP[…] log parser → results.jsonl
│   └── plot.py                # matplotlib (Pareto, scaling, crash)
├── bridges/
│   └── mysticeti_dpbridge/    # standalone Rust sidecar: Prometheus → DP[…]
├── local/                     # legacy local-tmux scripts (test4nodes.sh, etc.)
└── state/                     # gitignored: repos/ configs/ results/ aws.json
```

## Conventions

- **No upstream modifications.** Apollo and Mysticeti source trees are
  cloned into `state/repos/` at pinned SHAs and never edited. Where a
  protocol's native metric format differs (Mysticeti's Prometheus), a
  sidecar in `bridges/` translates to `DP[…]` externally.
- **Result format.** Every protocol's client process emits
  `DP[Throughput]: <f64>` and `DP[Latency]: <f64>` (ms) on stderr.
  Matches libapollo-rs's `consensus::statistics` output exactly so
  `orchestrator/parse.py` handles all five with one regex.
- **Reproducibility.** Every benchmark run writes
  `state/results/<stamp>/manifest.json` with per-protocol git SHAs,
  AWS region, instance type per role, and the full CLI args.

## Usage (planned)

```bash
# Local smoke (no AWS spend)
fab smoke --target local --protocol leto --num-nodes 4

# AWS provisioning
fab provision --num-nodes 6 --target aws
fab install --target aws
fab build --target aws

# Sweeps
fab bench --runs 3 --t 1 --protocols apollo,artemis,leto,zeus,mysticeti \
    --loads 1k,2.5k,5k,10k,25k,50k,100k,200k --tag pareto-v1

fab bench --runs 3 --t 1,2,5,10,20 --protocols apollo,artemis,leto,zeus,mysticeti \
    --load-mode saturating --tag scaling-v1

# Plots
fab plot --tag pareto-v1
fab plot --tag scaling-v1 --x-axis n

# Teardown
fab destroy --target aws
```

See `/Users/hermitsage/.claude/plans/magical-honking-muffin.md` for the
full design.
