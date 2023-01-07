use anyhow::{Context, Result};
use clap::{ArgGroup, Parser, ValueEnum};
use consensus::Id;
use crypto::Algorithm;
use std::path::PathBuf;

/// Top level Command
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Turn debugging information on
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbosity: u8,

    /// Optional: Logging parameters.
    /// This file contains the settings for logging
    #[arg(short, long, value_name = "FILE")]
    pub log_config: Option<PathBuf>,

    /// Runs a node type: server or client
    #[command(subcommand)]
    pub mode: SubCommand,
}

#[derive(Debug, Parser)]
pub enum SubCommand {
    /// Runs a server
    Server {
        /// The identity of the server (between 0 to n-1)
        #[arg(short, long)]
        #[arg(value_parser = str_to_id_parser)]
        id: Id,
        /// Config file (See examples/server.json for
        /// examples)
        #[arg(short, long, value_name = "FILE")]
        #[clap(default_value = "examples/server.json")]
        config: PathBuf,
        /// Keys file
        #[arg(short, long, value_name = "FILE")]
        key_file: PathBuf,
    },
    /// Runs a client
    Client {
        /// The identity of the client
        #[arg(short, long)]
        #[arg(value_parser = str_to_id_parser)]
        id: Id,
        /// Config file (See examples/client-4.json for
        /// examples)
        #[arg(short, long, value_name = "FILE")]
        #[clap(default_value = "examples/client.json")]
        config: PathBuf,
    },
    /// Generate keypairs for all the servers
    Keys {
        /// The number of servers for which we need to generate the keys
        #[arg(short, long)]
        num_servers: usize,

        /// The type of key to generate
        #[arg(short, long, value_enum)]
        #[clap(default_value_t = KeyType::ED25519)]
        key_type: KeyType,

        /// Output directory
        #[arg(short, long)]
        #[clap(default_value = ".")]
        output: PathBuf,
    },
    /// Generate/update configurations that apply to all the servers and clients
    #[command(group(
                ArgGroup::new("ip_group")
                    .args(["ip", "ip_file"]),
            ))]
    Config(CreateConfig),
}

#[derive(Debug, Parser)]
pub struct CreateConfig {
    /// Number of servers (n)
    #[arg(short = 'n', long)]
    pub servers: usize,

    /// The delay parameter in ms (Delta)
    #[arg(short = 'w', long)]
    #[clap(default_value_t = 500)]
    pub network_delay: u64,

    /// The Garbage collection depth
    #[arg(short, long)]
    #[clap(default_value_t = 5)]
    pub gc_depth: usize,

    /// The number of nodes to try to contact if a synchronization request fails
    #[arg(short, long)]
    #[clap(default_value_t = 4)]
    pub sync_retry_nodes: usize,

    /// The amount of time (in ms) to wait before re-sending a sync request
    #[arg(long)]
    #[clap(default_value_t = 1_000)]
    #[arg(short = 'd')]
    pub sync_retry_delay: u64,

    /// List of ips [default: 127.0.0.1]
    #[arg(short, long)]
    pub ip: Vec<String>,

    /// IP file: a file containing a list of ips
    #[arg(long)]
    #[arg(short = 'I')]
    pub ip_file: Option<PathBuf>,

    /// Local mode: If local, then the ports are assigned as consensus_port +
    /// id, mempool_port + id, client_port + id for all the servers
    #[arg(short, long)]
    #[clap(default_value_t = true)]
    pub local: bool,

    /// Mempool port (or base mempool port for local testing)
    #[arg(short = 'M', long)]
    #[clap(default_value_t = 7001)]
    #[arg(value_parser = clap::value_parser!(u16).range(1023..))]
    pub mempool_port: u16,

    /// Consensus port (or base consensus port for local testing)
    #[arg(long)]
    #[arg(short = 'C')]
    #[clap(default_value_t = 8001)]
    #[arg(value_parser = clap::value_parser!(u16).range(1023..))]
    pub consensus_port: u16,

    /// Client port for the servers (or base client port for local testing)
    #[arg(long)]
    #[arg(short = 'j')]
    #[clap(default_value_t = 10_001)]
    #[arg(value_parser = clap::value_parser!(u16).range(1023..))]
    pub client_port: u16,

    // /// Number of clients
    // #[arg(short = 'c', long)]
    // #[clap(default_value_t = 1)]
    // pub num_client: usize,
    /// The base directory to put the databases
    #[arg(short = 'b', long)]
    #[clap(default_value_t = format!("."))]
    pub db_base: String,

    /// The prefix of the databases
    #[arg(short = 'D', long)]
    #[clap(default_value_t = format!("db"))]
    pub db_prefix: String,

    /// Sized Sealer size in bytes
    #[arg(short = 'S', long)]
    #[clap(default_value_t = 1_024)]
    pub batch_size: usize,

    /// Timed Sealer time (in ms)
    #[arg(short = 't', long)]
    #[clap(default_value_t = 1_000)]
    pub max_batch_delay: u64,

    /// Optional ouptut directory for the Config
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Size of each transaction
    #[arg(short = 'm', long)]
    #[clap(default_value_t = 1_024)]
    #[arg(value_parser = clap::value_parser!(u64).range(33..))]
    pub tx_size: usize,

    /// Burst interval (in ms)
    #[arg(short = 'B', long)]
    #[clap(default_value_t = 50)]
    pub burst_interval: u64,

    /// Number of transactions in every burst
    #[arg(short = 'e', long)]
    #[clap(default_value_t = 100)]
    pub txs_per_burst: usize,
}

#[derive(Debug, ValueEnum, Clone)]
pub enum SealerType {
    Timed,
    Sized,
    Hybrid,
}

fn str_to_id_parser(id_str: &str) -> Result<Id> {
    Id::from_str_radix(id_str, 10)
        .map_err(anyhow::Error::new)
        .context("Error parsing ID string (Must be a decimal number)")
}

#[derive(Debug, ValueEnum, Clone)]
pub enum KeyType {
    SECP256K1,
    ED25519,
}

impl From<Algorithm> for KeyType {
    fn from(alg: Algorithm) -> Self {
        match alg {
            Algorithm::RSA => unimplemented!(),
            Algorithm::ED25519 => Self::ED25519,
            Algorithm::SECP256K1 => Self::SECP256K1,
        }
    }
}

impl From<KeyType> for Algorithm {
    fn from(kt: KeyType) -> Algorithm {
        match kt {
            KeyType::ED25519 => Algorithm::ED25519,
            KeyType::SECP256K1 => Algorithm::SECP256K1,
        }
    }
}
