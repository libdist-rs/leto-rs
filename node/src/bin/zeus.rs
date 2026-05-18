/// Zeus node binary.
///
/// Parallel to `node/src/main.rs` (Leto) but uses `ZeusServer::spawn`.
///
/// Usage: node-zeus server --id <ID> --config <CONFIG> --key-file <KEY>
use anyhow::{anyhow, Result};
use clap::Parser;
use consensus::{
    server::{DummyCommitSink, Settings, ZeusServer},
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

const APP_NAME: &str = "ZEUS_NODE";
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
            info!("Zeus: using config file {}", config_file);

            let settings = Settings::new(config_file)?;
            let all_ids = settings.committee_config.get_all_ids();

            let key_reader = std::fs::File::open(key_file)?;
            let crypto_system: KeyConfig = serde_json::from_reader(key_reader)?;

            let (tx_commit, rx_commit) = unbounded_channel();
            DummyCommitSink::spawn(rx_commit);

            let exit_tx =
                ZeusServer::<TestTx>::spawn(id, all_ids, crypto_system, settings, tx_commit)?;

            let mut signals = Signals::new(&[SIGINT, SIGTERM])?;
            signals.forever().next();
            info!("Zeus: received termination signal");
            exit_tx
                .send(())
                .map_err(|_| anyhow!("Zeus server already shut down"))?;
        }
        SubCommand::Client { .. } => {
            anyhow::bail!(
                "Zeus clients use the same interface as Leto. \
                 Run `node client --config <CONFIG>` instead."
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
                info!("Zeus: wrote key {} to {}", i, file_name.display());
            }
        }
        SubCommand::Config(cfg) => {
            println!("Zeus uses the same config format as Leto. Run `node config` to generate.");
            let _ = cfg;
        }
    }

    info!("Shutting down {}", APP_NAME);
    Ok(())
}
