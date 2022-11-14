use anyhow::{anyhow, Result};
use clap::{ArgGroup, Parser, ValueEnum};
use consensus::{
    client::{self, Stressor},
    server,
    server::{BenchConfig, Server, StorageConfig},
    Id, Round,
};
use fnv::FnvHashMap;
use log::*;
use log4rs::{
    append::console::ConsoleAppender,
    config::{Appender, Root},
    encode::pattern::PatternEncoder,
    Config,
};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use std::io::BufReader;
use std::{fs::File, io::BufRead, path::PathBuf, time::Duration};
const APP_NAME: &'static str = "LETO_NODE";
const DEFAULT_LOG_LEVEL: Level = Level::Info;

/*
 * TODO: Allow updation of setups
 * TODO: Test file based logging
 */

/// Top level Command
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Turn debugging information on
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbosity: u8,

    /// Optional: Logging parameters.
    /// This file contains the settings for logging
    #[arg(short, long, value_name = "FILE")]
    log_config: Option<PathBuf>,

    /// Runs a node type: server or client
    #[command(subcommand)]
    mode: SubCommand,
}

#[derive(Debug, ValueEnum, Clone)]
enum SealerType {
    Timed,
    Sized,
    Hybrid,
}

#[derive(Debug, Parser)]
struct CreateConfig {
    /// Number of servers (n)
    #[arg(short, long)]
    num_servers: usize,

    /// The Garbage collection depth
    #[arg(short, long)]
    #[clap(default_value_t = 5)]
    gc_depth: usize,

    /// The number of nodes to try to contact if a synchronization request fails
    #[arg(short, long)]
    #[clap(default_value_t = 4)]
    sync_retry_nodes: usize,

    /// The amount of time (in ms) to wait before re-sending a sync request
    #[arg(long)]
    #[clap(default_value_t = 1_000)]
    #[arg(short = 'd')]
    sync_retry_delay_ms: u64,

    /// List of ips [default: 127.0.0.1]
    #[arg(short, long)]
    ip: Vec<String>,

    /// IP file: a file containing a list of ips
    #[arg(long)]
    #[arg(short = 'I')]
    ip_file: Option<PathBuf>,

    /// Local mode: If local, then the ports are assigned as consensus_port +
    /// id, mempool_port + id, client_port + id for all the servers
    #[arg(short, long)]
    #[clap(default_value_t = true)]
    local: bool,

    /// Mempool port (or base mempool port for local testing)
    #[arg(short = 'M', long)]
    #[clap(default_value_t = 7000)]
    #[arg(value_parser = clap::value_parser!(u16).range(1023..))]
    mempool_port: u16,

    /// Consensus port (or base consensus port for local testing)
    #[arg(long)]
    #[arg(short = 'C')]
    #[clap(default_value_t = 8000)]
    #[arg(value_parser = clap::value_parser!(u16).range(1023..))]
    consensus_port: u16,

    /// Client port for the servers (or base client port for local testing)
    #[arg(long)]
    #[arg(short = 'P')]
    #[clap(default_value_t = 9000)]
    #[arg(value_parser = clap::value_parser!(u16).range(1023..))]
    server_client_port: u16,

    /// Client port for the servers (or base client port for local testing)
    #[arg(long)]
    #[arg(short = 'j')]
    #[clap(default_value_t = 10_000)]
    #[arg(value_parser = clap::value_parser!(u16).range(1023..))]
    client_port: u16,

    /// Number of clients
    #[arg(short = 'c', long)]
    #[clap(default_value_t = 1)]
    num_client: usize,

    /// The base directory to put the databases
    #[arg(short = 'b', long)]
    #[clap(default_value_t = format!("."))]
    db_base: String,

    /// The prefix of the databases
    #[arg(short = 'D', long)]
    #[clap(default_value_t = format!("db"))]
    db_prefix: String,

    /// Type of block sealer
    #[arg(short = 'T', long, value_enum)]
    #[clap(default_value_t = SealerType::Sized)]
    sealer_type: SealerType,

    /// Sized Sealer size in bytes
    #[arg(short = 'S', long)]
    #[clap(default_value_t = 1_024)]
    sealer_size: usize,

    /// Timed Sealer time (in ms)
    #[arg(short = 't', long)]
    #[clap(default_value_t = 1_000)]
    timeout_ms: u64,

    /// Optional ouptut directory for the Config
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Size of each transaction
    #[arg(short = 'm', long)]
    #[clap(default_value_t = 1_024)]
    tx_size: usize,

    /// Burst interval (in ms)
    #[arg(short = 'B', long)]
    #[clap(default_value_t = 50)]
    burst_interval_ms: u64,

    /// Number of transactions in every burst
    #[arg(short = 'e', long)]
    #[clap(default_value_t = 100)]
    txs_per_burst: usize,
}

#[derive(Debug, Parser)]
enum Setup {
    /// Create Setup files
    #[command(group(
                ArgGroup::new("ip_group")
                    .args(["ip", "ip_file"]),
            ))]
    Create(CreateConfig),
    /// Update existing configs to new values
    Update {
        /// Optional ouptut directory for the Config
        #[arg(short, long, value_name = "FILE")]
        #[clap(default_value = ".")]
        source: PathBuf,
    },
}

fn str_to_id_parser(id_str: &str) -> Result<Id, String> {
    Id::try_from(id_str).map_err(|e| e.to_string())
}

// fn str_to_atleast_parser(atleast: usize, num_server_str: &str) ->
// Result<usize, String> {
//     let val = num_server_str.parse::<usize>().map_err(|e| e.to_string())?;
//     if val < atleast {
//         return Err(format!("{} is too small. Use atleast {}.", val,
// atleast));     }
//     Ok(val)
// }

#[derive(Debug, Parser)]
enum SubCommand {
    /// Runs a server
    Server {
        /// The identity of the server (between 0 to n-1)
        #[arg(short, long)]
        #[arg(value_parser = str_to_id_parser)]
        id: Id,
        /// Config file (See consensus/src/server/test/Default.toml for
        /// examples)
        #[arg(short, long, value_name = "FILE")]
        #[clap(default_value = "consensus/src/server/test/Default.toml")]
        config: PathBuf,
    },
    /// Runs a client
    Client {
        /// The identity of the client
        #[arg(short, long)]
        #[arg(value_parser = str_to_id_parser)]
        id: Id,
        /// Config file (See consensus/src/client/test/Default.toml for
        /// examples)
        #[arg(short, long, value_name = "FILE")]
        #[clap(default_value = "consensus/src/client/test/Default.toml")]
        config: PathBuf,
    },
    /// Generate/update configurations
    #[command(subcommand)]
    Config(Setup),
}

// generate a default stdout logger
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

// generate a logger with configuration provided in `log_file`
fn logger_from_file(
    level: log::Level,
    log_file: PathBuf,
) -> Result<()> {
    let level_filter = level.to_level_filter();

    let mut logger_conf = log4rs::config::load_config_file(log_file, Default::default())?;
    logger_conf.root_mut().set_level(level_filter);
    log4rs::init_config(logger_conf)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("Starting {} with {:?}", APP_NAME, std::env::args());

    // Parse the args
    let args = Cli::parse();

    // Count the verbosity and use it to set the log level
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
        _ => format!("Other"),
    };

    // Setup logging
    match args.log_config {
        // Use config file if some file is specified
        Some(log_file) => logger_from_file(log_level, log_file),
        // Use default logger
        None => {
            default_logger(id_str, log_level)?;
            Ok(())
        }
    }?;

    match args.mode {
        SubCommand::Server { id, config } => {
            let server_id = id;

            // Get the config file, or use a default config file
            let config_file = config
                .to_str()
                .ok_or(anyhow!(format!("Unknown file: {}", config.display())))?
                .to_string();
            info!("Using config file: {}", config_file);

            let settings = server::Settings::new(config_file)?;
            info!("Using the settings: {:?}", settings);

            let all_ids = settings.consensus_config.get_all_ids();

            // Start the Server
            let _exit_tx = Server::spawn(server_id, all_ids, settings)?;

            // Implement a waiting strategy
            let mut signals = Signals::new(&[SIGINT, SIGTERM])?;
            signals.forever().next();
            info!("Received termination signal");
            info!("Shutting down server");
        }
        SubCommand::Client { id, config } => {
            let client_id = id;

            // Get the config file, or use a default config file
            let config_file = config
                .to_str()
                .ok_or(anyhow!(format!("Unknown file: {}", config.display())))?
                .to_string();
            info!("Using config file: {}", config_file);

            let settings = client::Settings::new(config_file)?;
            info!("Using the settings: {:?}", settings);

            // Start the client
            let _exit_tx = Stressor::spawn(client_id, settings)?;
            // Implement a waiting strategy
            let mut signals = Signals::new(&[SIGINT, SIGTERM])?;
            signals.forever().next();
            info!("Received termination signal");
            info!("Shutting down server");
        }
        SubCommand::Config(Setup::Update { .. }) => todo!(),
        SubCommand::Config(Setup::Create(mut config)) => {
            // Create mempool config
            let mut mempool_config = mempool::Config::<Round>::default();
            mempool_config.sync_retry_nodes = config.sync_retry_nodes;
            mempool_config.gc_depth = config.gc_depth.into();
            mempool_config.sync_retry_delay = Duration::from_millis(config.sync_retry_delay_ms);

            // Create consensus config
            let mut ids = Vec::<Id>::with_capacity(config.num_servers);
            let mut server_parties = FnvHashMap::default();
            let mut client_parties = FnvHashMap::default();
            for i in 0..config.num_servers {
                ids.push(i.into());
            }
            if config.local {
                while config.ip.len() < config.num_servers {
                    config.ip.push(format!("127.0.0.1"));
                }
            } else if config.ip.len() != config.num_servers {
                if config.ip_file.is_none() {
                    return Err(anyhow!("local: false, but ip file is not provided"));
                }
                let ip_file = config.ip_file.unwrap();
                let f = File::open(ip_file)?;
                let buf_reader = BufReader::new(f);
                let mut reader = buf_reader.lines();
                while let Some(Ok(line)) = reader.next() {
                    let line = line.replace(" ", "");
                    config.ip.push(line);
                    if config.ip.len() == config.num_servers {
                        break;
                    }
                }
            }
            for i in 0..config.num_servers {
                let consensus_port = if config.local {
                    config.consensus_port + i as u16
                } else {
                    config.consensus_port
                };
                let mempool_port = if config.local {
                    config.mempool_port + i as u16
                } else {
                    config.mempool_port
                };
                let server_client_port = if config.local {
                    config.server_client_port + i as u16
                } else {
                    config.server_client_port
                };
                server_parties.insert(
                    ids[i],
                    server::Party {
                        id: ids[i],
                        consensus_address: config.ip[i].clone(),
                        consensus_port,
                        mempool_address: config.ip[i].clone(),
                        mempool_port,
                        client_port: server_client_port,
                    },
                );
                client_parties.insert(
                    ids[i],
                    client::Party {
                        id: ids[i],
                        address: config.ip[i].clone(),
                        port: server_client_port,
                    },
                );
            }
            let consensus_config = server::Config {
                parties: server_parties,
            };

            // Create Storage config
            let storage_config = StorageConfig {
                base: config.db_base,
                prefix: config.db_prefix,
            };

            // Create Bench config
            let bench_config = match config.sealer_type {
                SealerType::Sized => BenchConfig {
                    sealer: server::SealerType::Sized {
                        size: config.sealer_size,
                    },
                },
                SealerType::Timed => BenchConfig {
                    sealer: server::SealerType::Timed {
                        timeout_ms: config.timeout_ms,
                    },
                },
                SealerType::Hybrid => BenchConfig {
                    sealer: server::SealerType::Hybrid {
                        timeout_ms: config.timeout_ms,
                        size: config.sealer_size,
                    },
                },
            };

            // Create server settings from this information
            let server_settings = server::Settings {
                mempool_config,
                consensus_config,
                storage: storage_config,
                bench_config: Some(bench_config),
            };
            // Create client settings from this information
            let mut client_settings = Vec::with_capacity(config.num_client);
            for i in 0..config.num_client {
                let client_port = if config.local {
                    config.client_port + i as u16
                } else {
                    config.client_port
                };
                client_settings.push(client::Settings {
                    port: client_port,
                    bench_config: client::Bench {
                        burst_interval_ms: config.burst_interval_ms,
                        tx_size: config.tx_size,
                        txs_per_burst: config.txs_per_burst,
                    },
                    consensus_config: client::Config {
                        parties: client_parties.clone(),
                    },
                });
            }

            if let Some(output_dir) = config.output {
                // Output to files
                let mut server_file = output_dir.clone();
                server_file.push("server.json");
                let server_file_name = server_file.display().to_string();
                let server_file_writer = File::create(server_file)?;
                serde_json::to_writer_pretty(server_file_writer, &server_settings)?;
                info!(
                    "Wrote the server config file successfully to {}",
                    server_file_name
                );

                // Output client settings
                for i in 0..config.num_client {
                    let mut client_file = output_dir.clone();
                    client_file.push(format!("client-{}.json", config.num_servers + i));
                    let client_file_name = client_file.display().to_string();
                    let client_file = File::create(client_file)?;
                    serde_json::to_writer_pretty(client_file, &client_settings[i])?;
                    info!(
                        "Wrote the client config file successfully to {}",
                        client_file_name
                    );
                }
            } else {
                // Print the settings
                // Can be used as a dry-run to verify before writing the configs
                println!(
                    "Server settings: {}",
                    serde_json::to_string_pretty(&server_settings)?
                );
                println!(
                    "Client settings: {}",
                    serde_json::to_string_pretty(&client_settings)?
                );
            }
        }
    };

    info!("Shutting down {}", APP_NAME);
    Ok(())
}
