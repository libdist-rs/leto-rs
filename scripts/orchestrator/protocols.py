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
    build_cmd="cargo build --release --bin node-apollo --bin client-apollo",
    node_run_cmd="{bin_dir}/node-apollo --config {config} --id {id}",
    client_run_cmd=(
        "{bin_dir}/client-apollo --config {config} --total-txs {total_txs} "
        "--window {window}"
    ),
    translator_module="orchestrator.translators.apollo",
)

ARTEMIS = Protocol(
    name="artemis",
    git_url="https://github.com/libdist-rs/libapollo-rs.git",
    git_sha="PINME",
    build_cmd="cargo build --release --bin node-artemis --bin client-artemis",
    node_run_cmd="{bin_dir}/node-artemis --config {config} --id {id}",
    client_run_cmd=(
        "{bin_dir}/client-artemis --config {config} --total-txs {total_txs} "
        "--window {window}"
    ),
    translator_module="orchestrator.translators.apollo",   # same config format
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
    build_cmd="cargo build --release -p mysticeti",
    # Mysticeti's own binary; flags TBD against the pinned SHA's CLI.
    node_run_cmd=(
        "{bin_dir}/mysticeti --committee {config} --authority {id} "
        "--metrics-address 0.0.0.0:1500 {extra}"
    ),
    # Mysticeti uses --dedicated-clients; the client binary's name and
    # exact flags depend on the SHA. PINME during install.
    client_run_cmd=(
        "{bin_dir}/mysticeti-client --committee {config} --rate {rate} "
        "--duration-secs {window}"
    ),
    sidecar_run_cmd=(
        # Built from scripts/bridges/mysticeti_dpbridge/.
        "{bridges_dir}/mysticeti_dpbridge/target/release/mysticeti-dpbridge "
        "--metrics-url {metrics_url} --interval-ms 1000"
    ),
    translator_module="orchestrator.translators.mysticeti",
    instance_type="c8g.xlarge",   # bumped per profile; revisit after smoke
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
