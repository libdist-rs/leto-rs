"""Canonical committee → Hera config files.

Hera shares the leto-rs `consensus::server::Settings` struct with Leto/Zeus, so
this translator delegates the server-side JSON to `translators/leto.py` and
then deletes the client-side files leto.translate writes (Hera has no external
client process — every node self-generates load via the TPS env var).
"""

from __future__ import annotations

from pathlib import Path

from orchestrator.genconfig import Committee
from orchestrator.translators import leto as leto_translator


def translate(
    committee: Committee, out_dir: Path, protocol: str = "hera"
) -> dict[str, Path]:
    """Write `hera-server.json` (delegated to leto) and nothing else.

    Returns the same shape dict as leto.translate but filtered to only the
    server-side entries (Hera has no client driver process).
    """
    paths = leto_translator.translate(committee, out_dir, protocol="hera")

    # Drop any hera-client-*.json files leto.translate wrote; Hera has no
    # external client process.
    filtered: dict[str, Path] = {}
    for label, p in paths.items():
        if label.startswith("client-"):
            try:
                p.unlink()
            except FileNotFoundError:
                pass
        else:
            filtered[label] = p
    return filtered
