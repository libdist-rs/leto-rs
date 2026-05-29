#!/usr/bin/env python3
"""Generate hera-server.json + key files for a local 32-node run.

Mirrors gen_hera_n61.py (same bench settings) with N=32.
Port layout (stride=100, base=18000): node i consensus=18000+i*100, etc.
"""
import json
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..'))
from orchestrator.genconfig import generate_local
from orchestrator.translators import hera as hera_translator
from pathlib import Path

N = 4
F = (N - 1) // 3   # BFT: f = floor((n-1)/3) = 10
OUT = Path(__file__).parent / "hera-n4-config"

committee = generate_local(n=N, f=F, num_clients=0, base_port=18000)

paths = hera_translator.translate(committee, OUT, protocol="hera")

server_path = paths["server"]
cfg = json.loads(server_path.read_text())
cfg["bench_config"]["batch_size"] = 500_000
cfg["bench_config"]["delay_in_ms"] = 2000
cfg["bench_config"]["bench_emit_window_secs"] = 5
cfg["bench_config"]["bench_metrics_node"] = 0
cfg["mempool_config"]["sync_retry_nodes"] = N
server_path.write_text(json.dumps(cfg, indent=2))

print(f"Wrote {server_path}")
print(f"n={N} f={F}")
print(f"Next: run node-hera keys --output {OUT} --num-servers {N}")
