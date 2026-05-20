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

import concurrent.futures as cf
import shutil
import subprocess
import time
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

    Note: SFTP from a home network is single-stream and uplink-bound.
    For multi-host distribution prefer `fanout_from_node0` which uploads
    once to node 0 over the home uplink and then fans out over the AWS
    internal network.
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


def fanout_from_node0(
    c0,
    others: list,
    protocol: "Protocol",
    remote_root: str,
    node0_private_ip: str,
) -> bool:
    """Distribute the cached tarball to a whole cluster efficiently.

    Strategy:
      1. SFTP-upload the local cache tarball to node 0 (one upload over
         the user's home uplink — the only slow leg).
      2. Untar on node 0 into `<remote_root>/bin/<protocol>/`.
      3. Start a one-shot `python3 -m http.server` on node 0 in a tmux
         session, serving the tarball to the AWS internal network.
      4. Have every other host `curl` the tarball over the same-AZ AWS
         backbone (~5 Gbps, sub-second per ~100 MB), untar, delete.
      5. Tear down the http server on node 0.

    Requires the cluster's security group to allow intra-SG traffic on
    the chosen port (the leto-bench SG opens all intra-SG traffic).

    Returns True on cache hit + successful fanout; False if no cache
    entry exists for this (protocol, sha).
    """
    src = cache_path(protocol.name, protocol.git_sha)
    if not src.exists():
        return False

    remote_tar = f"/tmp/{protocol.name}-bin.tar.gz"
    # Step 1 — single upload from home uplink → node 0.
    c0.put(str(src), remote_tar)
    # Step 2 — untar on node 0.
    c0.run(
        f"mkdir -p {remote_root}/bin "
        f"&& tar xzf {remote_tar} -C {remote_root}/bin",
        hide=True,
    )

    if not others:
        c0.run(f"rm -f {remote_tar}", hide=True)
        return True

    # Stable per-protocol port (avoids collisions if two fanouts ever
    # overlap, though install_remote runs them serially).
    port = 18000 + (sum(ord(ch) for ch in protocol.name) % 100)
    serve_dir = f"/tmp/binstore_{protocol.name}_serve"
    session = f"binstore-{protocol.name}"

    # Step 3 — move tarball into a clean serve dir + start http.server
    # under tmux so we can kill it reliably afterwards.
    c0.run(
        f"rm -rf {serve_dir} && mkdir -p {serve_dir} && "
        f"mv {remote_tar} {serve_dir}/bin.tar.gz && "
        f"tmux kill-session -t {session} 2>/dev/null || true; "
        f"tmux new-session -d -s {session} -n http "
        f"'cd {serve_dir} && python3 -m http.server {port}'",
        hide=True,
    )
    # Give python3 a moment to bind the socket.
    time.sleep(1)

    # Step 4 — fan out over AWS internal network in parallel.
    def _fetch(c):
        c.run(
            f"curl -sSf --retry 5 --retry-delay 1 "
            f"http://{node0_private_ip}:{port}/bin.tar.gz "
            f"-o /tmp/{protocol.name}-bin.tar.gz && "
            f"mkdir -p {remote_root}/bin && "
            f"tar xzf /tmp/{protocol.name}-bin.tar.gz -C {remote_root}/bin && "
            f"rm -f /tmp/{protocol.name}-bin.tar.gz",
            hide=True,
        )

    try:
        with cf.ThreadPoolExecutor(max_workers=max(1, len(others))) as ex:
            list(ex.map(_fetch, others))
    finally:
        # Step 5 — always clean up the http server + serve dir, even on
        # partial failure, so a retried run isn't blocked by a stale port.
        c0.run(
            f"tmux kill-session -t {session} 2>/dev/null || true; "
            f"rm -rf {serve_dir}",
            hide=True,
            warn=True,
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
