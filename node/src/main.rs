use anyhow::Result;
use clap::{Parser, Subcommand};
use consensus::{
    server, client,
    server::Server,
    Id,
};
use log::*;
use log4rs::{
    append::console::ConsoleAppender,
    config::{Appender, Root},
    Config,
};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
const APP_NAME: &'static str = "LETO_NODE";
const DEFAULT_LOG_LEVEL: Level = Level::Info;

/*
 * TODO: Allow creation of setups
 * TODO: Allow updation of setups
 * TODO: Add the node ID to the logger so we can trace logs more easily
 * TODO: Test file based logging
 * TODO: Handle client program
 *
 *
 */

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Turn debugging information on
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbosity: u8,

    /// Optional: Logging parameters.
    /// This file contains the settings for logging
    #[arg(short, long, value_name = "FILE")]
    log_config: Option<String>,

    /// Runs a node type: server or client
    #[command(subcommand)]
    mode: SubCommand,
}

#[derive(Debug, Subcommand)]
enum SubCommand {
    /// Runs a server
    Server {
        /// The identity of the server
        #[arg(short, long)]
        id: String,
        /// Config directory
        #[arg(short, long, value_name = "FILE")]
        config: Option<String>,
    },
    /// Runs a client
    Client {
        /// The identity of the client
        #[arg(short, long)]
        id: String,
        /// Config directory
        #[arg(short, long, value_name = "FILE")]
        config: Option<String>,
    },
    /// Generate configurations
    Setup {},
}

// generate a default stdout logger
fn default_logger(level: log::Level) -> Result<log4rs::Handle> {
    let level_filter = level.to_level_filter();
    let stdout = ConsoleAppender::builder().build();

    let config = Config::builder()
        .appender(Appender::builder().build("stdout", Box::new(stdout)))
        .build(Root::builder().appender("stdout").build(level_filter))?;

    Ok(log4rs::init_config(config)?)
}

// generate a logger with configuration provided in `log_file`
fn logger_from_file(
    level: log::Level,
    log_file: String,
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

    // Setup logging
    match args.log_config {
        // Use config file if some file is specified
        Some(log_file) => logger_from_file(log_level, log_file),
        // Use default logger
        None => {
            default_logger(log_level)?;
            Ok(())
        }
    }?;

    match args.mode {
        SubCommand::Server { id, config } => {
            let server_id = Id::try_from(id)?;

            // Get the config file, or use a default config file
            let config_file =
                config.unwrap_or("consensus/src/server/test/Default.toml".to_string());
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
            info!("Shutting down node");
        }
        SubCommand::Client { id, config } => {
            // TODO: Use this
            let _client_id = Id::try_from(id)?;

            // Get the config file, or use a default config file
            let config_file =
                config.unwrap_or("consensus/src/client/test/Default.toml".to_string());
            info!("Using config file: {}", config_file);

            let settings = client::Settings::new(config_file)?;
            info!("Using the settings: {:?}", settings);
        }
        _ => {
            todo!()
        }
    };

    info!("Shutting down {}", APP_NAME);
    Ok(())
}
