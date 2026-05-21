"""AWS provisioning via boto3.

Defaults from [[project_zeus_steady_state]] + the leto-aws-experimenter
agent:
- Instance type: c8g.large (Graviton 4, 2 vCPU, 4 GiB, EBS-only).
- Region/AZ: us-west-2d (cheapest spot).
- AMI: Amazon Linux 2023 arm64 (owner: amazon, name filter
  `al2023-ami-2023.*-kernel-*-arm64`). REMOTE_USER is `ec2-user`.
- On-demand by default (spot=False) for paper-grade run guarantees.

State persisted to scripts/state/aws.json across fab invocations so
provision/install/build/bench/destroy can hand off cleanly.
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Optional


DEFAULT_REGION = "us-west-2"
DEFAULT_AZ = "us-west-2d"
# c8g.xlarge on-demand: $0.0796/hr × 6 hosts = $0.48/hr cluster.  Spot
# would be ~$0.046/hr × 6 = $0.28/hr but risks mid-sweep interruption.
# For paper-grade runs we trade ~$0.20/hr for guaranteed completion.
DEFAULT_INSTANCE_TYPE = "c8g.xlarge"
DEFAULT_SPOT = False
# 30 GB gp3 root volume on every host so Mysticeti's LLVM codegen
# doesn't OOM (AL2023's default 8 GB filled up during the first build).
# gp3 is ~$0.08/GB-month → 30 GB × 6 hosts × $0.08 = ~$14/month if you
# leave the volumes around; trivial for short-lived benchmark clusters.
DEFAULT_ROOT_VOLUME_GB = 30
# Amazon Linux 2023 (arm64) — free, owned by Amazon, no Marketplace
# subscription required.  Matches libapollo-rs's existing fabfile.py
# bootstrap pattern (dnf-based) so reproducibility across the two
# orchestrators stays cheap.
DEFAULT_AMI_OWNER = "amazon"
DEFAULT_AMI_NAME_FILTER = "al2023-ami-2023.*-kernel-*-arm64"
DEFAULT_TAG = "leto-bench"


@dataclass
class Instance:
    instance_id: str
    public_ip: Optional[str]
    private_ip: str
    role: str                       # "node" or "client"
    instance_type: str
    az: str


def state_path() -> Path:
    return Path(__file__).resolve().parent.parent / "state" / "aws.json"


def load_state() -> dict:
    p = state_path()
    if not p.exists():
        return {"instances": [], "region": DEFAULT_REGION}
    return json.loads(p.read_text())


def save_state(state: dict) -> None:
    p = state_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(state, indent=2))


def _ec2_client(region: str):
    import boto3   # imported lazily so the module loads without boto3 installed
    return boto3.client("ec2", region_name=region)


def _resolve_ami(region: str) -> str:
    """Look up the latest Canonical Ubuntu 24.04 arm64 AMI by name."""
    ec2 = _ec2_client(region)
    images = ec2.describe_images(
        Owners=[DEFAULT_AMI_OWNER],
        Filters=[
            {"Name": "name", "Values": [DEFAULT_AMI_NAME_FILTER]},
            {"Name": "state", "Values": ["available"]},
        ],
    )["Images"]
    if not images:
        raise RuntimeError(f"no AMI matched {DEFAULT_AMI_NAME_FILTER}")
    images.sort(key=lambda i: i["CreationDate"], reverse=True)
    return images[0]["ImageId"]


def _list_live_by_tag(ec2, tag: str) -> list[dict]:
    """Return raw EC2 Instance dicts (running + pending) matching tag:Project=<tag>."""
    resp = ec2.describe_instances(Filters=[
        {"Name": "tag:Project", "Values": [tag]},
        {"Name": "instance-state-name", "Values": ["running", "pending"]},
    ])
    return [i for r in resp.get("Reservations", []) for i in r.get("Instances", [])]


def _role_from_tags(inst: dict) -> Optional[str]:
    for t in inst.get("Tags", []) or []:
        if t.get("Key") == "Role":
            return t.get("Value")
    return None


def _wait_and_fill(ec2, instances: list[Instance]) -> None:
    """Wait until instances are running + status_ok, then populate IPs in place."""
    ids = [i.instance_id for i in instances]
    if not ids:
        return
    ec2.get_waiter("instance_running").wait(InstanceIds=ids)
    ec2.get_waiter("instance_status_ok").wait(InstanceIds=ids)
    described = ec2.describe_instances(InstanceIds=ids)["Reservations"]
    by_id: dict[str, dict] = {}
    for r in described:
        for inst in r["Instances"]:
            by_id[inst["InstanceId"]] = inst
    for i in instances:
        live = by_id[i.instance_id]
        i.public_ip = live.get("PublicIpAddress")
        i.private_ip = live.get("PrivateIpAddress", i.private_ip)


def _launch_batch(
    ec2,
    ami_id: str,
    role: str,
    count: int,
    instance_type: str,
    az: str,
    spot: bool,
    key_name: str,
    security_group_id: str,
    subnet_id: str,
    tag: str,
    root_volume_gb: int,
) -> list[Instance]:
    """run_instances for one role's worth of hosts. Role-tagged at create
    so subsequent idempotent provisions can recover the node/client split
    without consulting aws.json."""
    if count <= 0:
        return []
    market_options = (
        {"MarketType": "spot", "SpotOptions": {"SpotInstanceType": "one-time"}}
        if spot
        else None
    )
    run_args = {
        "ImageId": ami_id,
        "InstanceType": instance_type,
        "MinCount": count,
        "MaxCount": count,
        "KeyName": key_name,
        "SecurityGroupIds": [security_group_id],
        "SubnetId": subnet_id,
        "Placement": {"AvailabilityZone": az},
        "BlockDeviceMappings": [{
            "DeviceName": "/dev/xvda",
            "Ebs": {
                "VolumeSize": root_volume_gb,
                "VolumeType": "gp3",
                "DeleteOnTermination": True,
            },
        }],
        "TagSpecifications": [{
            "ResourceType": "instance",
            "Tags": [
                {"Key": "Name", "Value": f"{tag}-{role}-{instance_type}"},
                {"Key": "Project", "Value": tag},
                {"Key": "Role", "Value": role},
            ],
        }],
    }
    if market_options:
        run_args["InstanceMarketOptions"] = market_options
    resp = ec2.run_instances(**run_args)
    out: list[Instance] = []
    for inst in resp["Instances"]:
        out.append(Instance(
            instance_id=inst["InstanceId"],
            public_ip=None,
            private_ip=inst.get("PrivateIpAddress", ""),
            role=role,
            instance_type=instance_type,
            az=az,
        ))
    return out


def provision(
    # NOTE: the cluster is homogeneous by design — every host (nodes +
    # clients, every protocol) runs on `instance_type`. Per-protocol
    # `Protocol.instance_type` overrides in orchestrator/protocols.py are
    # NOT consulted here; the fair cross-protocol comparison depends on
    # identical hardware. If a future sweep needs heterogeneous instance
    # types, split the provision call by role.
    num_nodes: int,
    num_clients: int,
    instance_type: str = DEFAULT_INSTANCE_TYPE,
    az: str = DEFAULT_AZ,
    spot: bool = DEFAULT_SPOT,
    key_name: Optional[str] = None,
    security_group_id: Optional[str] = None,
    subnet_id: Optional[str] = None,
    tag: str = DEFAULT_TAG,
    root_volume_gb: int = DEFAULT_ROOT_VOLUME_GB,
) -> list[Instance]:
    """Idempotent EC2 provisioning.

    Behavior:
    1. Query EC2 for live instances tagged ``Project=<tag>`` in this region.
    2. If the live set already matches the requested shape (count,
       instance_type, az, role split — derived from each instance's
       ``Role`` tag), refresh aws.json from EC2 truth and return.
       No new instances launched.
    3. If a live set exists but mismatches the request — count, type, az,
       or any instance lacks the ``Role`` tag — refuse and tell the
       caller to either ``fab destroy`` or use a different ``--tag`` so
       the two clusters don't pile up. (Previously this path silently
       launched a second cluster, doubling billing and orphaning the
       original.)
    4. Otherwise launch fresh: two ``run_instances`` calls so each host
       gets a precise ``Role={node,client}`` tag at create time.

    Caller must have AWS credentials configured (env or ~/.aws/).
    Requires an existing key_name + security_group_id + subnet_id in
    the chosen region.
    """
    if key_name is None or security_group_id is None or subnet_id is None:
        raise ValueError(
            "key_name, security_group_id, and subnet_id must be supplied. "
            "Create them once via `aws ec2 create-key-pair`, "
            "`create-security-group`, `create-subnet` and pass via fab args."
        )
    region = az[:-1]   # us-west-2d → us-west-2
    ec2 = _ec2_client(region)
    total = num_nodes + num_clients

    # ---- Idempotency: reuse-or-refuse ---------------------------------
    live = _list_live_by_tag(ec2, tag)
    if live:
        types = {i.get("InstanceType") for i in live}
        azs = {i.get("Placement", {}).get("AvailabilityZone") for i in live}
        roles = [_role_from_tags(i) for i in live]
        node_ids = [i["InstanceId"] for i, r in zip(live, roles) if r == "node"]
        client_ids = [i["InstanceId"] for i, r in zip(live, roles) if r == "client"]
        untagged = [i["InstanceId"] for i, r in zip(live, roles) if r is None]

        exact_match = (
            len(live) == total
            and types == {instance_type}
            and azs == {az}
            and len(node_ids) == num_nodes
            and len(client_ids) == num_clients
            and not untagged
        )
        if exact_match:
            print(
                f"provision: reusing {total} existing instances tagged Project={tag} "
                f"({num_nodes} nodes + {num_clients} clients, {instance_type} in {az})"
            )
            instances: list[Instance] = []
            for inst in live:
                role = _role_from_tags(inst) or "node"
                instances.append(Instance(
                    instance_id=inst["InstanceId"],
                    public_ip=inst.get("PublicIpAddress"),
                    private_ip=inst.get("PrivateIpAddress", ""),
                    role=role,
                    instance_type=inst.get("InstanceType", instance_type),
                    az=inst.get("Placement", {}).get("AvailabilityZone", az),
                ))
            # Sort nodes first then clients (launch_remote expects this order).
            instances.sort(key=lambda i: (0 if i.role == "node" else 1, i.instance_id))
            _wait_and_fill(ec2, instances)
            save_state({
                "region": region,
                "az": az,
                "instance_type": instance_type,
                "spot": spot,
                "tag": tag,
                "instances": [asdict(i) for i in instances],
            })
            return instances

        # Mismatch — refuse to add to the pile.
        details = (
            f"found {len(live)} live instance(s) tagged Project={tag}:\n"
            f"  types={types}, azs={azs}, "
            f"nodes={len(node_ids)}, clients={len(client_ids)}, "
            f"untagged={len(untagged)}\n"
            f"requested: {num_nodes} nodes + {num_clients} clients "
            f"({instance_type} in {az})"
        )
        suggestion = (
            "Resolve one of:\n"
            "  (a) `fab destroy --config <toml>` to terminate the existing cluster\n"
            "  (b) re-run with --tag <other> to provision a parallel cluster\n"
            "  (c) match the request to the live cluster (same counts/type/az)"
        )
        raise RuntimeError(f"provision: cluster shape mismatch.\n{details}\n{suggestion}")

    # ---- No live set — fresh launch -----------------------------------
    ami_id = _resolve_ami(region)
    nodes = _launch_batch(
        ec2, ami_id, "node", num_nodes,
        instance_type, az, spot, key_name, security_group_id, subnet_id,
        tag, root_volume_gb,
    )
    clients = _launch_batch(
        ec2, ami_id, "client", num_clients,
        instance_type, az, spot, key_name, security_group_id, subnet_id,
        tag, root_volume_gb,
    )
    instances = nodes + clients
    _wait_and_fill(ec2, instances)

    state = {
        "region": region,
        "az": az,
        "instance_type": instance_type,
        "spot": spot,
        "tag": tag,
        "instances": [asdict(i) for i in instances],
    }
    save_state(state)
    return instances


def status() -> None:
    """Print provisioned instances + hourly cost estimate."""
    s = load_state()
    if not s.get("instances"):
        print("no provisioned instances; state empty")
        return
    instance_type = s.get("instance_type", DEFAULT_INSTANCE_TYPE)
    n = len(s["instances"])
    # Per project_perf_findings, c8g.large spot median ≈ $0.022/hr.
    # Hard-coded estimate; for true price use aws-cli describe-spot-price-history.
    estimate = 0.022 * n if instance_type == "c8g.large" else 0.05 * n
    print(f"region/az: {s.get('region')}/{s.get('az')}")
    print(f"type:      {instance_type} ({'spot' if s.get('spot') else 'on-demand'})")
    print(f"instances: {n}  (~${estimate:.2f}/hr estimate)")
    for inst in s["instances"]:
        print(
            f"  {inst['role']:6} {inst['instance_id']}  "
            f"private={inst['private_ip']:15}  public={inst.get('public_ip') or '-':15}"
        )


def destroy(tag: str = DEFAULT_TAG) -> None:
    """Terminate every live instance tagged Project=<tag>. Idempotent.

    Sourced from EC2 (tag query), NOT just aws.json — so orphans left
    by a crashed provision or out-of-band launch get cleaned up too.
    The state.json is reset on success.
    """
    s = load_state()
    region = s.get("region", DEFAULT_REGION)
    ec2 = _ec2_client(region)
    live = _list_live_by_tag(ec2, tag)
    ids = [i["InstanceId"] for i in live]
    # Also include anything in state.json (defensive: handles a stale
    # state pointing at instances that lost their Project tag somehow).
    for inst in s.get("instances", []):
        if inst["instance_id"] not in ids:
            ids.append(inst["instance_id"])
    if not ids:
        print("no instances to destroy")
        save_state({"instances": [], "region": region})
        return
    ec2.terminate_instances(InstanceIds=ids)
    save_state({"instances": [], "region": region})
    print(f"destroy initiated for {len(ids)} instance(s)")


def scale_down(target_nodes: int, target_clients: int) -> int:
    """Terminate excess instances beyond `target_nodes` + `target_clients`.

    Used by reverse-scaling sweeps: provision the largest cluster once,
    run the largest-t row, then incrementally trim the cluster as t
    shrinks so idle hosts stop billing.

    Preserves the first `target_nodes` node instances and the first
    `target_clients` client instances (the same ones bench.py uses
    when only the leading N hosts of the cluster are needed for a
    given t).  Returns the number of instances terminated.  Idempotent.
    """
    s = load_state()
    instances = s.get("instances", [])
    nodes = [i for i in instances if i.get("role") == "node"]
    clients = [i for i in instances if i.get("role") == "client"]

    keep_nodes = nodes[:target_nodes]
    keep_clients = clients[:target_clients]
    terminate = nodes[target_nodes:] + clients[target_clients:]

    if not terminate:
        print(
            f"scale_down: cluster already at {len(nodes)} nodes + "
            f"{len(clients)} clients (target {target_nodes}n + "
            f"{target_clients}c); nothing to do"
        )
        return 0

    region = s.get("region", DEFAULT_REGION)
    ec2 = _ec2_client(region)
    ids = [i["instance_id"] for i in terminate]
    ec2.terminate_instances(InstanceIds=ids)

    s["instances"] = keep_nodes + keep_clients
    save_state(s)

    print(
        f"scale_down: terminated {len(terminate)} instance(s); kept "
        f"{len(keep_nodes)} nodes + {len(keep_clients)} clients"
    )
    return len(terminate)
