use crate::Id;
use fnv::FnvHashMap as HashMap;
use serde::{Deserialize, Serialize};
use std::env;

/// Controls how the `Stressor` routes transactions to server nodes.
///
/// `LetoBroadcast` replicates the original Leto behaviour: every tx is fanned
/// out to all servers and each server runs its own mempool/batcher.
///
/// `ZeusEleaderOnly` implements the correct Zeus client model (zeus.tex §8.1):
/// the client sends all txs only to the eleader.  The eleader id is provided
/// directly (harness fast-path) or resolved via a `WhoIsEleader` request on
/// startup.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum ClientMode {
    /// Fan-out to all servers (Leto default).
    LetoBroadcast,
    /// Send only to the eleader for the given epoch (Zeus).
    ///
    /// `eleader_id` is `Some(id)` when the harness already knows the eleader
    /// (fast-path; no query needed), or `None` when the stressor should resolve
    /// it via `WhoIsEleader` at startup.
    ZeusEleaderOnly { eleader_id: Option<Id> },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Party {
    pub id: Id,
    pub address: String,
    /// Port for sending transactions, corresponds to the client_port of the
    /// server
    pub port: u16,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    /// All the parties in the system
    pub parties: HashMap<Id, Party>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
/// Benchmark configurations
pub struct Bench {
    /// The amount of bytes to send
    /// Must be above 8 bytes as we will add a tag
    pub tx_size: usize,
    /// Every `burst_interval_ms`, `txs_per_burst` transactions are sent to all
    /// the servers
    pub burst_interval_ms: u64,
    /// Every `burst_interval_ms`, `txs_per_burst` transactions are sent to all
    /// the servers
    pub txs_per_burst: usize,
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
pub struct Settings {
    pub consensus_config: Config,
    pub bench_config: Bench,
    /// Controls how this stressor routes transactions.
    ///
    /// Defaults to `LetoBroadcast` so that existing Leto harness code requires
    /// no change.
    #[serde(default = "Settings::default_client_mode")]
    pub client_mode: ClientMode,
}

impl Settings {
    fn default_client_mode() -> ClientMode {
        ClientMode::LetoBroadcast
    }
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
            .add_source(config::File::with_name(&run_mode).required(false))
            // ENV variables override the file settings
            // For example LETO_LOG
            .add_source(
                config::Environment::with_prefix("LETO_CLIENT")
                    .try_parsing(true)
                    .separator("_")
                    .list_separator(" "),
            )
            .build()?;
        conf.try_deserialize().map_err(anyhow::Error::new)
    }
}
