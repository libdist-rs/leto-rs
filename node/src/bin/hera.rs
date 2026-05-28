/// Hera node binary.
///
/// Parallel to `node/src/bin/zeus.rs` but uses `HeraServer::spawn`.
/// No `SubCommand::Client` arm — load is generated internally via the `TPS`
/// env var.
///
/// Usage: node-hera server --id <ID> --config <CONFIG> --key-file <KEY>
///
/// Environment:
///   TPS=<n>   If set and > 0, each node generates n transactions per second
///             internally via load_gen::spawn.  Default: 0 (no self-load).
use anyhow::{anyhow, Result};
use clap::Parser;
use consensus::{
    server::{DummyCommitSink, HeraServer, Settings},
    KeyConfig,
};
use log::*;
use log4rs::{
    append::console::ConsoleAppender,
    config::{Appender, Root},
    encode::pattern::PatternEncoder,
    Config,
};
use node::{Cli, SimpleData, SimpleTx, SubCommand};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use tokio::sync::mpsc::unbounded_channel;

const APP_NAME: &str = "HERA_NODE";
const DEFAULT_LOG_LEVEL: log::Level = log::Level::Info;

type TestTx = SimpleTx<SimpleData>;

fn default_logger(
    id: String,
    level: log::Level,
) -> Result<log4rs::Handle> {
    let level_filter = level.to_level_filter();
    let log_str = format!(
        "{{f}}:{{L}} |NodeId:{} |{{d}} [{{l}}] {{h({{m}})}}{{n}}",
        id
    );
    let stdout = ConsoleAppender::builder()
        .encoder(Box::new(PatternEncoder::new(&log_str)))
        .build();
    let config = Config::builder()
        .appender(Appender::builder().build("stdout", Box::new(stdout)))
        .build(Root::builder().appender("stdout").build(level_filter))?;
    Ok(log4rs::init_config(config)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    // PROFILE INSTRUMENTATION: tokio-console init.
    // Gate behind HERA_CONSOLE=1 so only one node in a 61-node run listens.
    // Port = TOKIO_CONSOLE_BASE_PORT (default 6669) + node_id, resolved later
    // from argv. We initialise the subscriber before log4rs so the tokio
    // tracing layer is active from the start. Requires building with
    // RUSTFLAGS="--cfg tokio_unstable" and the `console` feature.
    #[cfg(feature = "console")]
    {
        if std::env::var("HERA_CONSOLE").as_deref() == Ok("1") {
            // Determine node id from argv to pick a unique port.
            let console_port: u16 = {
                let args: Vec<String> = std::env::args().collect();
                let id_pos = args.iter().position(|a| a == "--id");
                let id: u64 = id_pos
                    .and_then(|p| args.get(p + 1))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let base: u16 = std::env::var("TOKIO_CONSOLE_BASE_PORT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(6669);
                base.saturating_add(id as u16)
            };
            let addr = format!("127.0.0.1:{}", console_port);
            console_subscriber::ConsoleLayer::builder()
                .server_addr(addr.parse::<std::net::SocketAddr>().unwrap())
                .init();
            eprintln!("tokio-console listening on {}", addr);
        }
    }

    // Raise the per-process FD soft limit.
    match fdlimit::raise_fd_limit() {
        Ok(fdlimit::Outcome::LimitRaised { from, to }) => {
            println!("Raised FD limit: {} → {}", from, to);
        }
        Ok(fdlimit::Outcome::Unsupported) => {
            println!("FD limit raise: unsupported on this platform");
        }
        Err(e) => {
            eprintln!("FD limit raise failed: {e}; continuing with current limit");
        }
    }

    println!("Starting {} with {:?}", APP_NAME, std::env::args());

    let args = Cli::parse();

    let log_level = match args.verbosity {
        0 => DEFAULT_LOG_LEVEL,
        1 => log::Level::Error,
        2 => log::Level::Warn,
        3 => log::Level::Info,
        4 => log::Level::Debug,
        _ => log::Level::Trace,
    };

    let id_str = match &args.mode {
        SubCommand::Server { id, .. } => id.to_string(),
        SubCommand::Client { id, .. } => id.to_string(),
        _ => "Other".to_string(),
    };

    match args.log_config {
        Some(log_file) => {
            let level_filter = log_level.to_level_filter();
            let mut conf = log4rs::config::load_config_file(log_file, Default::default())?;
            conf.root_mut().set_level(level_filter);
            log4rs::init_config(conf)?;
        }
        None => {
            default_logger(id_str, log_level)?;
        }
    }

    match args.mode {
        SubCommand::Server {
            id,
            config,
            key_file,
        } => {
            let config_file = config
                .to_str()
                .ok_or_else(|| anyhow!("Invalid config path"))?
                .to_string();
            info!("Hera: using config file {}", config_file);

            let settings = Settings::new(config_file)?;
            let all_ids = settings.committee_config.get_all_ids();

            let key_reader = std::fs::File::open(key_file)?;
            let crypto_system: KeyConfig = serde_json::from_reader(key_reader)?;

            let tps: usize = std::env::var("TPS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let tx_size: usize = std::env::var("TX_SIZE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(512);
            info!("Hera: TPS={} TX_SIZE={}", tps, tx_size);

            let (tx_commit, rx_commit) = unbounded_channel();
            DummyCommitSink::spawn(rx_commit);

            let make_tx = move |my_id: consensus::Id, nonce: u64, now_ns: u128| -> TestTx {
                let mut payload = vec![0u8; tx_size.max(32)];
                payload[..8].copy_from_slice(&(my_id as u64).to_le_bytes());
                payload[8..16].copy_from_slice(&nonce.to_le_bytes());
                payload[16..32].copy_from_slice(&now_ns.to_le_bytes());
                TestTx {
                    data: <SimpleData as node::Data>::with_payload(&payload),
                    source: my_id,
                    nonce,
                    extra: Vec::new(),
                }
            };

            let (exit_tx, _max_heads) = HeraServer::<TestTx>::spawn_with_factory(
                id,
                all_ids,
                crypto_system,
                settings,
                tx_commit,
                Some(make_tx),
            )?;

            let mut signals = Signals::new(&[SIGINT, SIGTERM])?;
            signals.forever().next();
            info!("Hera: received termination signal");
            exit_tx
                .send(())
                .map_err(|_| anyhow!("Hera server already shut down"))?;
        }
        SubCommand::Client { .. } => {
            anyhow::bail!(
                "Hera has no external client. Load is generated internally via TPS env var."
            );
        }
        SubCommand::Keys {
            output,
            num_servers,
            key_type,
        } => {
            let key_configs = KeyConfig::generate(key_type.into(), num_servers)?;
            for (i, key) in key_configs.iter().enumerate() {
                let mut file_name = output.clone();
                file_name.push(format!("keys-{}.json", i));
                let out_file = std::fs::File::create(&file_name)?;
                serde_json::to_writer_pretty(out_file, key)?;
                info!("Hera: wrote key {} to {}", i, file_name.display());
            }
        }
        SubCommand::Config(cfg) => {
            println!(
                "Hera uses the same config format as Leto/Zeus. Run `node config` to generate."
            );
            let _ = cfg;
        }
    }

    info!("Shutting down {}", APP_NAME);
    Ok(())
}
