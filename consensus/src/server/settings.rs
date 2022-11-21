use crate::{Id, Round};
use fnv::FnvHashMap as HashMap;
use serde::{Deserialize, Serialize};
use std::{env, fmt, time::Duration};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StorageConfig {
    pub base: String,
    pub prefix: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ENV {
    Development,
    Testing,
    Production,
}

impl fmt::Display for ENV {
    fn fmt(
        &self,
        f: &mut fmt::Formatter,
    ) -> fmt::Result {
        match self {
            ENV::Development => write!(f, "Development"),
            ENV::Testing => write!(f, "Testing"),
            ENV::Production => write!(f, "Production"),
        }
    }
}

impl From<&str> for ENV {
    fn from(env: &str) -> Self {
        match env {
            "Testing" => ENV::Testing,
            "Production" => ENV::Production,
            _ => ENV::Development,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Party {
    pub id: Id,
    pub mempool_address: String,
    /// Port for mempool communication
    pub mempool_port: u16,
    pub consensus_address: String,
    /// Port for consensus communication
    pub consensus_port: u16,
    /// Port for clients to communicate
    pub client_port: u16,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    /// All the parties in the system
    pub parties: HashMap<Id, Party>,
}

impl Config {
    /// Returns the number of nodes in the consensus system
    pub fn num_nodes(&self) -> usize {
        self.parties.len()
    }

    /// Returns the party corresponding to Id
    pub fn get(
        &self,
        id: &Id,
    ) -> Option<&Party> {
        self.parties.get(id)
    }

    /// Returns all the parties
    pub fn get_all_ids(&self) -> Vec<Id> {
        self.parties.keys().cloned().collect()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BenchConfig {
    pub batch_size: usize,
    pub batch_timeout: Duration,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            batch_size: 1_000,
            batch_timeout: Duration::from_millis(1_000),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    /// Contains information about contacting the parties
    pub consensus_config: Config,
    /// Contains information about the mempool settings
    pub mempool_config: mempool::Config<Round>,
    pub storage: StorageConfig,
    /// Contains information about the sealing settings
    pub bench_config: BenchConfig,
}

impl Settings {
    pub fn new(config_file_name: String) -> anyhow::Result<Self> {
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "development".into());
        let conf = config::Config::builder()
            // DEFAULT settings Add in `./Settings.json`
            .add_source(config::File::with_name(&config_file_name).required(true))
            // Add in the current environment file (Testing, Dev or Prod)
            // Default to 'development' env
            // Note that this file is _optional_
            .add_source(config::File::with_name(&format!("{}", run_mode)).required(false))
            // ENV variables override the file settings
            // For example LETO_LOG
            .add_source(
                config::Environment::with_prefix("LETO")
                    .try_parsing(true)
                    .separator("_")
                    .list_separator(" "),
            )
            .build()?;
        conf.try_deserialize().map_err(|e| anyhow::Error::new(e))
    }
}
