"""TOML config loader for the multi-protocol benchmark orchestrator.

Each fab task can take an `--config <file>` flag.  When supplied, the
task reads its defaults from the file: cluster-wide settings live
under `[aws]`; per-task overrides live under sections matching the
task name (`[install]`, `[bench]`, ...).

CLI flags always win over config values.  Hardcoded defaults are used
only when neither the CLI nor the config provides a value.

`fab run --config <file>` executes `meta.steps` in declared order.
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path
from typing import Any


def load(path: str | Path) -> dict:
    """Read and parse a TOML config file."""
    p = Path(path).expanduser().resolve()
    if not p.exists():
        raise SystemExit(f"config file not found: {p}")
    with p.open("rb") as f:
        return tomllib.load(f)


def lookup(cfg: dict, *keys: str, default: Any = None) -> Any:
    """Walk a dotted path through a nested dict; return `default` if any
    segment is missing or non-dict."""
    node: Any = cfg
    for k in keys:
        if not isinstance(node, dict) or k not in node:
            return default
        node = node[k]
    return node


def cli_or(cli_val: Any, cfg: dict, *keys: str, default: Any = None) -> Any:
    """Return the CLI value if it is not None, else the config value at
    `keys`, else `default`."""
    if cli_val is not None:
        return cli_val
    return lookup(cfg, *keys, default=default)


def as_list(val: Any) -> list[str]:
    """Coerce a TOML list or a comma-separated string into a list."""
    if val is None:
        return []
    if isinstance(val, list):
        return [str(x) for x in val]
    return [x.strip() for x in str(val).split(",") if x.strip()]
