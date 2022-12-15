use anyhow::{anyhow, Result};
use clap::Parser;
use consensus::{
    client::{self, Stressor},
    server,
    server::{BenchConfig, DummyCommitSink, Server, StorageConfig},
    Id, KeyConfig, Round,
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
use tokio::sync::mpsc::unbounded_channel;
const APP_NAME: &'static str = "LETO_NODE";
const DEFAULT_LOG_LEVEL: Level = Level::Info;

/*
 * TODO: Test file based logging
 * TODO: Test RSA
 */

#[cfg(test)]
mod test;

mod cli;
pub use cli::*;

mod smr;
pub use smr::*;

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

fn write_config_files(
    output_dir: PathBuf,
    config: &CreateConfig,
    server_settings: &server::Settings,
    client_settings: &Vec<client::Settings>,
) -> Result<()> {
    let mut server_file = output_dir.clone();
    server_file.push("server.json");
    let server_file_name = server_file.display().to_string();
    json_write(server_file, &server_settings)?;
    info!(
        "Wrote the server config file successfully to {}",
        server_file_name
    );

    // Output client settings
    for i in 0..config.num_client {
        let mut client_file = output_dir.clone();
        client_file.push(format!("client-{}.json", config.num_servers + i));
        let client_file_name = client_file.display().to_string();
        json_write(client_file, &client_settings[i])?;
        info!(
            "Wrote the client config file successfully to {}",
            client_file_name
        );
    }
    Ok(())
}

fn create_settings(config: &CreateConfig) -> Result<(server::Settings, Vec<client::Settings>)> {
    // Create mempool config
    let mut mempool_config = mempool::Config::<Round>::default();
    mempool_config.sync_retry_nodes = config.sync_retry_nodes;
    mempool_config.gc_depth = config.gc_depth.into();
    mempool_config.sync_retry_delay = Duration::from_millis(config.sync_retry_delay_ms);

    // Create consensus config
    let mut ips = config.ip.clone();
    let mut ids = Vec::<Id>::with_capacity(config.num_servers);
    let mut server_parties = FnvHashMap::default();
    let mut client_parties = FnvHashMap::default();
    for i in 0..config.num_servers {
        ids.push(i.into());
    }
    if config.local {
        while ips.len() < config.num_servers {
            ips.push(format!("127.0.0.1"));
        }
    } else if ips.len() != config.num_servers {
        if config.ip_file.is_none() {
            return Err(anyhow!("local: false, but ip file is not provided"));
        }
        let ip_file = config.ip_file.clone().unwrap();
        let f = File::open(ip_file)?;
        let buf_reader = BufReader::new(f);
        let mut reader = buf_reader.lines();
        while let Some(Ok(line)) = reader.next() {
            let line = line.replace(" ", "");
            ips.push(line);
            if ips.len() == config.num_servers {
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
                consensus_address: ips[i].clone(),
                consensus_port,
                mempool_address: ips[i].clone(),
                mempool_port,
                client_port: server_client_port,
            },
        );
        client_parties.insert(
            ids[i],
            client::Party {
                id: ids[i],
                address: ips[i].clone(),
                port: server_client_port,
            },
        );
    }

    let committee_config = server::Config {
        parties: server_parties,
    };

    // Create Storage config
    let storage_config = StorageConfig {
        base: config.db_base.clone(),
        prefix: config.db_prefix.clone(),
    };

    // Create Bench config
    let bench_config = BenchConfig {
        batch_size: config.sealer_size,
        batch_timeout: Duration::from_millis(config.timeout_ms),
        delay_in_ms: config.delay_in_ms,
    };

    // Create server settings from this information
    let server_settings = server::Settings {
        mempool_config,
        committee_config,
        storage: storage_config,
        bench_config,
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
    Ok((server_settings, client_settings))
}

fn json_write(
    out_file: PathBuf,
    obj: &impl serde::Serialize,
) -> Result<()> {
    let out_file = File::create(out_file)?;
    serde_json::to_writer_pretty(out_file, obj)?;
    Ok(())
}

fn create_keys(
    key_type: KeyType,
    num_servers: usize,
    output: PathBuf,
) -> Result<()> {
    let key_configs = KeyConfig::generate(key_type.into(), num_servers)?;
    for i in 0..num_servers {
        // Write the keys
        let mut file_name = output.clone();
        file_name.push(format!("keys-{}.json", i));
        let file_name_string = file_name.display().to_string();
        json_write(file_name, &key_configs[i])?;
        info!(
            "Successfully wrote key for node {} in {}",
            i, file_name_string
        );
    }
    Ok(())
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
        SubCommand::Server {
            id,
            config,
            key_file,
        } => {
            let server_id = id;

            // Get the config file, or use a default config file
            let config_file = config
                .to_str()
                .ok_or(anyhow!("Unknown file: {}", config.display()))?
                .to_string();
            info!("Using config file: {}", config_file);

            let settings = server::Settings::new(config_file)?;
            info!("Using the settings: {:?}", settings);

            let all_ids = settings.committee_config.get_all_ids();

            // Load the keys
            let key_reader = File::open(key_file)?;
            let crypto_system = serde_json::from_reader(key_reader)?;

            // Start the SMR
            let (tx_commit, rx_commit) = unbounded_channel();
            DummyCommitSink::spawn(rx_commit);

            // Start the Server
            let exit_tx = Server::<SimpleTx<SimpleData>>::spawn(
                server_id,
                all_ids,
                crypto_system,
                settings,
                tx_commit,
            )?;

            // Implement a waiting strategy
            let mut signals = Signals::new(&[SIGINT, SIGTERM])?;
            signals.forever().next();
            info!("Received termination signal");
            exit_tx
                .send(())
                .map_err(|_| anyhow!("Server already shut down"))?;
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
            let exit_tx = Stressor::<SimpleTx<SimpleData>>::spawn(client_id, settings)?;
            // Implement a waiting strategy
            let mut signals = Signals::new(&[SIGINT, SIGTERM])?;
            signals.forever().next();
            info!("Received termination signal");
            exit_tx
                .send(())
                .map_err(|_| anyhow!("Client already shut down"))?;
            info!("Shutting down client");
        }
        SubCommand::Keys {
            output,
            num_servers,
            key_type,
        } => {
            create_keys(key_type, num_servers, output)?;
        }
        SubCommand::Config(config) => {
            let (server_settings, client_settings) = create_settings(&config)?;
            if let Some(output_dir) = config.output.clone() {
                // Output to files
                write_config_files(output_dir, &config, &server_settings, &client_settings)?;
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
