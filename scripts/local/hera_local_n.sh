#!/usr/bin/env bash
# Launch N hera nodes locally (oversubscribed) for profiling.
# Usage: N=61 TPS=2000 bash hera_local_n.sh
# Environment:
#   N          — number of nodes (default 61)
#   TPS        — per-node transactions per second (default 2000)
#   TX_SIZE    — tx size in bytes (default 512)
#   LOG_DIR    — log directory (default ./hera-n61-logs)
#   HERA_CONSOLE — if set to 1, enable tokio-console on node 0 only
#   HERA_ROUND_TIMER_MS — blame timer override (default 8000ms for local/oversubscribed)

set -euo pipefail

N=${N:-61}
TPS=${TPS:-2000}
TX_SIZE=${TX_SIZE:-512}
LOG_DIR=${LOG_DIR:-"$(pwd)/hera-n${N}-logs"}
CONFIG_DIR=${CONFIG_DIR:-"$(pwd)/scripts/local/hera-n${N}-config"}
CONFIG="$CONFIG_DIR/hera-server.json"
KEYS_DIR="$CONFIG_DIR"
BIN="$(pwd)/target/release/node-hera"

# Wider blame timer to survive oversubscription context-switch jitter.
HERA_ROUND_TIMER_MS=${HERA_ROUND_TIMER_MS:-8000}
# Wait for n-1 peers before starting (but cap at 90s for local; nodes start sequentially).
HERA_STARTUP_GATE_CAP_MS=${HERA_STARTUP_GATE_CAP_MS:-90000}
# Start at n-f-1 peers (quorum minus one fault) to avoid waiting forever on an
# oversubscribed machine where the last few connections are slow.
# For n=61, f=20: quorum = (61+20+1)/2 = 41. Use 40 as the gate.
HERA_STARTUP_GATE_PEERS=${HERA_STARTUP_GATE_PEERS:-40}

echo "=== Hera local N=$N TPS=$TPS TX_SIZE=$TX_SIZE ==="
echo "=== Config: $CONFIG ==="
echo "=== Logs: $LOG_DIR ==="

mkdir -p "$LOG_DIR"

# Raise FD limit for 61 processes each holding O(n) sockets.
ulimit -n 10240 || true

# Kill any previous instances.
pkill -f "node-hera server" || true
sleep 1

# Remove old databases.
rm -f "$(pwd)"/db-*.db

echo "Launching $N node-hera processes..."
for ((i=0; i<N; i++)); do
    # Only enable tokio-console on node 0 if requested.
    CONSOLE_ENV=""
    if [[ "${HERA_CONSOLE:-0}" == "1" && "$i" == "0" ]]; then
        CONSOLE_ENV="HERA_CONSOLE=1"
    fi

    env \
        TPS="$TPS" \
        TX_SIZE="$TX_SIZE" \
        HERA_ROUND_TIMER_MS="$HERA_ROUND_TIMER_MS" \
        HERA_STARTUP_GATE_CAP_MS="$HERA_STARTUP_GATE_CAP_MS" \
        HERA_STARTUP_GATE_PEERS="$HERA_STARTUP_GATE_PEERS" \
        ${CONSOLE_ENV} \
        "$BIN" server \
            --id "$i" \
            --config "$CONFIG" \
            --key-file "$KEYS_DIR/keys-$i.json" \
        > "$LOG_DIR/node-$i.log" 2>&1 &
done

echo "All $N processes launched. Watching node-0 log (Ctrl-C to stop)..."
echo "Monitor: tail -f $LOG_DIR/node-0.log | grep -E 'HB:|CHAN_DEPTH|NET_DROPS|DP\['"
echo ""
echo "To kill: pkill -f 'node-hera server'"
