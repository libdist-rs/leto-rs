# Hera server architecture

Every box below is a `tokio::spawn` task; every edge is a channel (or shared
atomic, dashed). Source references are `file:line` in `consensus/src/server/hera/`.

## Actors & channels

```mermaid
flowchart TB
  classDef task fill:#1f6feb,stroke:#0b3a8a,color:#fff;
  classDef shed fill:#8957e5,stroke:#4b277d,color:#fff;
  classDef ext  fill:#2da44e,stroke:#106629,color:#fff;
  classDef atom fill:#bf8700,stroke:#7a5600,color:#fff;

  %% ───────────── TRANSPORT (hera::net) ─────────────
  subgraph NET["hera::net — persistent-connection transport"]
    direction TB
    SRV["Server task<br/>accept loop<br/>select{accept, stop}<br/><i>network.rs:150</i>"]:::task
    HS["handshake task<br/>(per inbound socket, 10s)<br/><i>network.rs:161</i>"]:::task
    WK["Worker task ×(n-1)<br/>select(work, recv socket)<br/>1–5s jittered reconnect<br/><i>network.rs:206</i>"]:::task
    STREAM["handle_stream<br/>select_all[write_fut, read_fut]<br/>30s ping/pong, break RTT&gt;5s<br/><i>network.rs:267</i>"]:::task
    ROUTER["HeraNet router task<br/>updates peers map,<br/>connected counter<br/><i>handle.rs:58</i>"]:::task
    PUMP["inbound-pump task ×live conns<br/>receiver → inbound_tx<br/><i>handle.rs:76</i>"]:::task
  end

  SRV -->|"worker_senders[id]<br/>unbounded&lt;TcpStream&gt;"| WK
  WK --> STREAM
  STREAM -->|"network_out mpsc(1000)<br/>OUTBOUND try_send DROP"| STREAM
  WK -->|"connection_sender<br/>mpsc(16)"| ROUTER
  ROUTER --> PUMP
  STREAM -->|"network_in mpsc(1000)<br/>INBOUND (blocking send)"| PUMP

  %% ───────────── CONSENSUS CORE ─────────────
  DESER["Deserializer task<br/>bincode → HeraMsg,<br/>split by is_data_plane()<br/><i>core.rs:324</i>"]:::task
  PUMP -->|"inbound_rx mpsc(1000)&lt;Bytes&gt;"| DESER

  MAIN["MAIN TASK — Hera::run()<br/>tokio::select! { biased; ... }<br/><i>core.rs:504</i>"]:::task

  DESER -->|"tx_sig_net unbounded<br/>(SigPropose/Blame/BlameQC/<br/>SigElement) — PRIORITY"| MAIN
  DESER -->|"tx_data_net unbounded<br/>(DataPropose/Request/Response)<br/>— lowest priority"| MAIN

  MAIN -->|"tx_msg_loopback unbounded<br/>(self-deliver)"| MAIN
  MAIN -.->|"consensus_net.broadcast/send()<br/>peers map, try_send"| ROUTER

  %% commit context
  COMMIT["HeraCommitContext task<br/>walk sig-chain,<br/>(n+f+1)/2 unique proposers<br/><i>commit.rs:71</i>"]:::task
  MAIN -->|"tx_inner unbounded<br/>HeraCommitMsg::EndRound"| COMMIT
  COMMIT -->|"tx_committed unbounded<br/>HeraCommittedAttestation"| MAIN

  %% batcher + load
  BATCH["RRBatcher task<br/>batch by size/timeout<br/><i>rr_batcher.rs:80</i>"]:::task
  MAIN -->|"tx_consensus_to_batcher<br/>unbounded (NewRound/Committed)"| BATCH
  BATCH -->|"tx_data_batch unbounded&lt;Batch&gt;"| MAIN

  LOAD["Load-gen task<br/>TPS-paced, 100ms windows<br/>self-pace via atomic<br/><i>load_gen.rs:49</i>"]:::task
  MEM["Mempool tasks (libmempool)<br/>client listener idle"]:::ext
  LOAD -->|"rx_mem_to_batcher<br/>unbounded&lt;(Tx,usize)&gt;"| BATCH
  MEM  -->|"tx_mem_to_batcher"| BATCH

  %% outputs / exit
  SMR["SMR / caller"]:::ext
  MAIN -->|"tx_data_commit<br/>unbounded&lt;Arc&lt;Batch&gt;&gt;"| SMR
  EXIT(["HeraServer exit_tx"]):::ext
  EXIT -->|"exit_rx oneshot"| MAIN

  %% shared atomics (dashed)
  CONN[["connected: AtomicUsize"]]:::atom
  MYC[["my_committed_txs: AtomicU64"]]:::atom
  ROUTER -.->|"++ on first conn"| CONN
  CONN -.->|"startup gate reads"| MAIN
  MAIN -.->|"commit path ++"| MYC
  MYC -.->|"self-pacing read"| LOAD
```

## Main select loop (biased — top wins)

```mermaid
flowchart LR
  classDef br fill:#0d1117,stroke:#30363d,color:#e6edf3;
  L["select! { biased }"]:::br
  L --> B1["1 · exit_rx (oneshot) → break"]:::br
  L --> B2["2 · timer.tick() [timer_enabled]<br/>→ on_round_timeout (blame)"]:::br
  L --> B3["3 · rx_committed.recv()<br/>→ on_committed_attestation"]:::br
  L --> B4["4 · rx_msg_loopback.recv() → dispatch"]:::br
  L --> B5["5 · rx_sig_net.recv() → dispatch ⟵ PRIORITY"]:::br
  L --> B6["6 · async{} [round_state.is_ready()]<br/>→ handle_sig_ordered (pop buffered)"]:::br
  L --> B7["7 · rx_data_batch.recv() → on_self_propose"]:::br
  L --> B8["8 · rx_data_net.recv() → dispatch ⟵ lowest"]:::br
  L --> B9["9 · bench_emit_interval.tick() [emit_dp]<br/>→ DP[Throughput] / DP[Latency]"]:::br
```

## Key design points

- **Priority-class split (core.rs:43):** the deserializer fans one inbound
  stream into two channels by `is_data_plane()`. The `biased` select drains
  control + sig-plane (branches 1–6) before the all-to-all data flood
  (7–8), so the leader's `SigPropose`/`Blame`/`BlameQC` never sit behind it.
  This is the large-`n` sig-chain wedge fix.
- **Shedding transport (handle.rs:93):** outbound is `try_send` on bounded(1000)
  per-peer channels, dropping on full. One persistent connection per peer pair
  + jittered reconnect kills the libnet reconnect storm. Asymmetric: the inbound
  read path (network.rs:373) does a *blocking* send — backpressure, not drop.
- **`round_state` (branch 6) is not a channel:** it's an in-memory `VecDeque` +
  `future_msgs` map that `dispatch` writes; branch 6 is a zero-cost `async {}`
  guard popping one buffered sig message per iteration when `is_ready()`.
