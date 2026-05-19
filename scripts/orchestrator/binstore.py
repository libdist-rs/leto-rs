"""Local binary cache for cluster provisioning.

After a successful `fab build`, the per-protocol binaries on node 0 are
tarballed and pulled back to `scripts/state/cache/<protocol>-<sha>.tar.gz`
on the user's local machine.  On the next `fab install`, if a cache
entry exists for a protocol's pinned SHA, the tarball is pushed to
every host and untarred into `~/leto-bench/bin/<protocol>/`, skipping
the clone + build entirely.

Why local-disk cache and not S3:
- The IAM user `macbook-personal` lacks `s3:CreateBucket` /
  `iam:CreateRole` perms (tried; got AccessDenied).  Asking for those
  perms requires admin access to the account.
- A local-disk cache requires zero new AWS perms and is reliable.
  The trade-off: must build at least once per dev machine, vs
  S3 where any cluster anywhere can pull.
- Tarballs are small (~20 MB per protocol post-strip) so SCP-up at
  provision time is ~3 s per host vs ~5–10 min of build per protocol.

Cache layout (gitignored):
    scripts/state/cache/
        apollo-9320466.tar.gz
        artemis-9320466.tar.gz
        leto-0fb04e3.tar.gz
        zeus-0fb04e3.tar.gz
        mysticeti-3b78b12.tar.gz
        mysticeti_dpbridge-<orchestrator-sha>.tar.gz

Each tarball is the contents of `~/leto-bench/bin/<protocol>/` on the
host (i.e. the binaries themselves, no source).  The SHA in the
filename is the same `Protocol.git_sha` the orchestrator's registry
pins.  Bumping a pinned SHA invalidates the cache for that protocol
naturally.
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path
from typing import Optional, TYPE_CHECKING

if TYPE_CHECKING:
    from orchestrator.protocols import Protocol


def cache_root() -> Path:
    p = Path(__file__).resolve().parent.parent / "state" / "cache"
    p.mkdir(parents=True, exist_ok=True)
    return p


def cache_path(protocol_name: str, sha: str) -> Path:
    """Where a given (protocol, sha) tarball lives in the local cache."""
    # Use a short SHA (first 7 chars) in the filename to keep paths
    # readable; the full SHA can collide only across truly different
    # commits with matching prefixes, which is fine for the small
    # number of protocols we cache.
    short = sha[:7] if len(sha) >= 7 else sha
    return cache_root() / f"{protocol_name}-{short}.tar.gz"


def is_cached(protocol: "Protocol") -> bool:
    return cache_path(protocol.name, protocol.git_sha).exists()


def pull_from_host(conn, protocol: "Protocol", remote_root: str) -> Path:
    """Tarball the protocol's bin dir on the remote host and pull to
    the local cache.

    `conn` is a fabric.Connection.  `remote_root` is the absolute path
    that holds `bin/<protocol>/` on the remote (typically
    `/home/ec2-user/leto-bench`).
    """
    remote_tar = f"/tmp/{protocol.name}-bin.tar.gz"
    conn.run(
        f"tar czf {remote_tar} -C {remote_root}/bin {protocol.name}",
        hide=True,
    )
    local_dest = cache_path(protocol.name, protocol.git_sha)
    if local_dest.exists():
        local_dest.unlink()
    conn.get(remote_tar, str(local_dest))
    conn.run(f"rm -f {remote_tar}", hide=True)
    return local_dest


def push_to_host(conn, protocol: "Protocol", remote_root: str) -> bool:
    """Push the cached tarball for this protocol to a remote host and
    untar into `<remote_root>/bin/<protocol>/`.

    Returns True if the cache was used, False if no entry existed.
    """
    src = cache_path(protocol.name, protocol.git_sha)
    if not src.exists():
        return False
    remote_tar = f"/tmp/{protocol.name}-bin.tar.gz"
    conn.put(str(src), remote_tar)
    conn.run(
        f"mkdir -p {remote_root}/bin "
        f"&& tar xzf {remote_tar} -C {remote_root}/bin "
        f"&& rm -f {remote_tar}",
        hide=True,
    )
    return True


def evict(protocol_name: str, sha: Optional[str] = None) -> int:
    """Remove cached tarball(s) for a protocol.

    If `sha` is None, evicts every entry for that protocol_name.
    Returns the number of files removed.
    """
    count = 0
    if sha is None:
        for p in cache_root().glob(f"{protocol_name}-*.tar.gz"):
            p.unlink()
            count += 1
    else:
        p = cache_path(protocol_name, sha)
        if p.exists():
            p.unlink()
            count += 1
    return count


def status() -> list[tuple[str, str, int]]:
    """Return [(protocol_name_or_filename, sha_short, size_bytes), ...]
    for every cache entry currently on disk.
    """
    out: list[tuple[str, str, int]] = []
    for p in sorted(cache_root().glob("*.tar.gz")):
        stem = p.stem.removesuffix(".tar")
        if "-" in stem:
            name, sha = stem.rsplit("-", 1)
        else:
            name, sha = stem, "?"
        out.append((name, sha, p.stat().st_size))
    return out
