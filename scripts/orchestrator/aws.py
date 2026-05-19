"""AWS provisioning via boto3.

Defaults from [[project_zeus_steady_state]] + the leto-aws-experimenter
agent:
- Instance type: c8g.large (Graviton 4, 2 vCPU, 4 GiB, EBS-only).
- Region/AZ: us-west-2d (cheapest spot).
- AMI: Ubuntu 24.04 LTS arm64, Canonical owner 099720109477.
- Spot by default.

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


def provision(
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
    """Launch EC2 instances; persist state to scripts/state/aws.json.

    Caller must have AWS credentials configured (env or ~/.aws/).
    Requires an existing key_name + security_group_id + subnet_id in
    the chosen region. provisioning a VPC/SG/subnet is out of scope of
    this skeleton — wire in fabfile.py's `provision` task or document
    one-time setup steps.
    """
    if key_name is None or security_group_id is None or subnet_id is None:
        raise ValueError(
            "key_name, security_group_id, and subnet_id must be supplied. "
            "Create them once via `aws ec2 create-key-pair`, "
            "`create-security-group`, `create-subnet` and pass via fab args."
        )
    region = az[:-1]   # us-west-2d → us-west-2
    ec2 = _ec2_client(region)
    ami_id = _resolve_ami(region)

    total = num_nodes + num_clients
    market_options = (
        {"MarketType": "spot", "SpotOptions": {"SpotInstanceType": "one-time"}}
        if spot
        else None
    )
    run_args = {
        "ImageId": ami_id,
        "InstanceType": instance_type,
        "MinCount": total,
        "MaxCount": total,
        "KeyName": key_name,
        "SecurityGroupIds": [security_group_id],
        "SubnetId": subnet_id,
        "Placement": {"AvailabilityZone": az},
        "BlockDeviceMappings": [{
            "DeviceName": "/dev/xvda",   # AL2023 root EBS device
            "Ebs": {
                "VolumeSize": root_volume_gb,
                "VolumeType": "gp3",
                "DeleteOnTermination": True,
            },
        }],
        "TagSpecifications": [{
            "ResourceType": "instance",
            "Tags": [
                {"Key": "Name", "Value": f"{tag}-{instance_type}"},
                {"Key": "Project", "Value": tag},
            ],
        }],
    }
    if market_options:
        run_args["InstanceMarketOptions"] = market_options

    resp = ec2.run_instances(**run_args)
    raw = resp["Instances"]
    instances: list[Instance] = []
    for i, inst in enumerate(raw):
        role = "node" if i < num_nodes else "client"
        instances.append(Instance(
            instance_id=inst["InstanceId"],
            public_ip=None,           # filled in after polling
            private_ip=inst.get("PrivateIpAddress", ""),
            role=role,
            instance_type=instance_type,
            az=az,
        ))

    # Wait for running + IP assignment
    waiter = ec2.get_waiter("instance_running")
    waiter.wait(InstanceIds=[i.instance_id for i in instances])
    described = ec2.describe_instances(
        InstanceIds=[i.instance_id for i in instances]
    )["Reservations"]
    by_id: dict[str, dict] = {}
    for r in described:
        for inst in r["Instances"]:
            by_id[inst["InstanceId"]] = inst
    for i in instances:
        live = by_id[i.instance_id]
        i.public_ip = live.get("PublicIpAddress")
        i.private_ip = live.get("PrivateIpAddress", i.private_ip)

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


def destroy() -> None:
    """Terminate all instances tracked in state. Idempotent."""
    s = load_state()
    ids = [i["instance_id"] for i in s.get("instances", [])]
    if not ids:
        print("no instances to destroy")
        return
    region = s.get("region", DEFAULT_REGION)
    ec2 = _ec2_client(region)
    ec2.terminate_instances(InstanceIds=ids)
    save_state({"instances": [], "region": region})
    print(f"destroy initiated for {len(ids)} instance(s)")
