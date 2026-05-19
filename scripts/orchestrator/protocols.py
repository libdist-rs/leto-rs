"""Per-protocol metadata.

One `Protocol` per benchmark target. The orchestrator clones each
protocol's source into `state/repos/<name>/` at the pinned SHA, builds
out-of-tree, runs nodes + clients per the templates, scrapes
`DP[Throughput]` and `DP[Latency]` lines from client stderr.

No upstream modifications: each protocol must already emit `DP[…]`
itself, or be paired with a sidecar in `scripts/bridges/` that does.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional


# ---------------------------------------------------------------------------
# Protocol metadata
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Protocol:
    """Everything the orchestrator needs to drive one protocol."""

    name: str
    git_url: str
    git_sha: str                          # pinned for reproducibility
    workspace_subdir: str = "."           # cargo workspace root inside the checkout

    # Build: invoked once per host inside the checkout.
    # Template; no substitutions yet.
    build_cmd: str = "cargo build --release"

    # Node run template. Substitutions: {config}, {id}, {bin_dir}, {extra}.
    node_run_cmd: str = "{bin_dir}/node-network --config {config} --id {id}"

    # Client run template. Substitutions: {config}, {rate}, {total_txs},
    # {window}, {bin_dir}, {extra}.
    client_run_cmd: str = (
        "{bin_dir}/client-network --config {config} --rate {rate} "
        "--total-txs {total_txs} --window {window}"
    )

    # Optional sidecar (e.g. Mysticeti's dpbridge). Substitutions:
    # {metrics_url}, {extra}.
    sidecar_run_cmd: Optional[str] = None

    # Translator: canonical committee dict → protocol's native config files
    # written under state/configs/<stamp>/<protocol>/.
    # Lazy import lookup so circular dependencies don't bite at module load.
    translator_module: str = ""

    # AWS instance type override. None ⇒ use orchestrator default
    # (c8g.large in us-west-2d).
    instance_type: Optional[str] = None

    # Free-form extra args appended verbatim to node + client commands.
    # Useful for protocol-specific tuning (batch size, delta, ...).
    node_extra_args: str = ""
    client_extra_args: str = ""

    # Build artifact relative to workspace root after `cargo build --release`.
    # Used to locate binaries for the run templates.
    bin_dir: str = "target/release"


# ---------------------------------------------------------------------------
# Registry — pinned SHAs to fill in when each protocol stabilises
# ---------------------------------------------------------------------------

# Note: SHAs marked PINME are placeholders. Pin against the actual HEAD
# at the time the first sweep is run; record in
# state/results/<stamp>/manifest.json.

APOLLO = Protocol(
    name="apollo",
    git_url="https://github.com/libdist-rs/libapollo-rs.git",
    git_sha="PINME",
    build_cmd="cargo build --release --bin node-apollo --bin client-apollo --bin genconfig",
    # libapollo-rs node-apollo CLI: `-c nodes-<i>.json -i ip_file -s --sleep N --delta MS`.
    # {node_config} and {ip_file} are substituted by deploy.launch_*.
    node_run_cmd=(
        "{bin_dir}/node-apollo -c {node_config} -i {ip_file} "
        "-s --sleep 5 --delta 50"
    ),
    # libapollo-rs client-apollo CLI: `-c client.json -i cli_ip_file -m <metrics> -w <window>`.
    # Apollo is single-client; ignore {id} / {rate}.
    client_run_cmd=(
        "{bin_dir}/client-apollo -c {client_config} -i {cli_ip_file} "
        "-m {total_txs} -w {window}"
    ),
    translator_module="orchestrator.translators.apollo",
)

ARTEMIS = Protocol(
    name="artemis",
    git_url="https://github.com/libdist-rs/libapollo-rs.git",
    git_sha="PINME",
    build_cmd="cargo build --release --bin node-artemis --bin client-artemis --bin genconfig",
    node_run_cmd=(
        "{bin_dir}/node-artemis -c {node_config} -i {ip_file} "
        "-s --sleep 5 --delta 50"
    ),
    client_run_cmd=(
        "{bin_dir}/client-artemis -c {client_config} -i {cli_ip_file} "
        "-m {total_txs} -w {window}"
    ),
    translator_module="orchestrator.translators.apollo",   # same genconfig output
)

LETO = Protocol(
    name="leto",
    git_url="https://github.com/libdist-rs/leto-rs.git",
    git_sha="PINME",
    build_cmd="cargo build --release --bin node",
    # `node` + `node server --id N --config <cfg> --key-file <key>` —
    # client mode reuses the same binary's `client` subcommand which
    # internally consumes the DP-enabled Stressor.
    node_run_cmd=(
        "{bin_dir}/node server --id {id} --config {config} "
        "--key-file {key_file}"
    ),
    client_run_cmd=(
        "{bin_dir}/node client --id {id} --config {config}"
    ),
    translator_module="orchestrator.translators.leto",
)

ZEUS = Protocol(
    name="zeus",
    git_url="https://github.com/libdist-rs/leto-rs.git",
    git_sha="PINME",
    build_cmd="cargo build --release --bin node-zeus --bin node",
    node_run_cmd=(
        "{bin_dir}/node-zeus server --id {id} --config {config} "
        "--key-file {key_file}"
    ),
    # Zeus shares Leto's `node client` (Stressor) — ClientMode in the
    # client config selects ZeusEleaderOnly routing.
    client_run_cmd=(
        "{bin_dir}/node client --id {id} --config {config}"
    ),
    translator_module="orchestrator.translators.leto",
)

MYSTICETI = Protocol(
    name="mysticeti",
    git_url="https://github.com/MystenLabs/mysticeti.git",
    git_sha="PINME",
    build_cmd="cargo build --release --bin mysticeti",
    # Mysticeti's single binary; `dry-run` self-generates committee +
    # keys + load (no separate client binary). TPS env controls the
    # built-in generator's rate.  Substitute {rate} into TPS at launch
    # time via the shell prefix.
    node_run_cmd=(
        "TPS={rate} {bin_dir}/mysticeti dry-run "
        "--committee-size {n} --authority {id}"
    ),
    # Mysticeti's dryrun mode has no separate client — the node binary
    # generates load internally.  We use a no-op client_run_cmd that
    # exits immediately; effective_clients=0 in deploy keeps it from
    # being launched.
    client_run_cmd="true",
    sidecar_run_cmd=(
        "{bridges_dir}/mysticeti_dpbridge/target/release/mysticeti-dpbridge "
        "--metrics-url {metrics_url} --interval-ms 1000 --max-secs {max_secs}"
    ),
    translator_module="orchestrator.translators.mysticeti",
    instance_type="c8g.xlarge",   # DAG nodes heavier than chain-BFT
)


REGISTRY: dict[str, Protocol] = {
    p.name: p for p in (APOLLO, ARTEMIS, LETO, ZEUS, MYSTICETI)
}


def get(name: str) -> Protocol:
    if name not in REGISTRY:
        raise KeyError(
            f"Unknown protocol {name!r}; known: {sorted(REGISTRY)}"
        )
    return REGISTRY[name]


def all_names() -> list[str]:
    return list(REGISTRY)
