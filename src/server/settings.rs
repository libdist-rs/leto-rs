use super::Round;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, fmt};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Log {
    pub file: String,
}

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
pub struct Config {
    pub mempool_addresses: HashMap<usize, (String, String)>,
    pub mempool_port: u16,
    // mempool config
    // net config
    // crypto config
    pub num_nodes: usize,
    pub consensus_port: u16,
    pub consensus_addresses: HashMap<usize, (String, String)>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    pub mempool_config: mempool::Config<Round>,
    pub consensus_config: Config,
    pub storage: StorageConfig,
}

impl Settings {
    pub fn new() -> anyhow::Result<Self> {
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "development".into());
        let conf = config::Config::builder()
            // DEFAULT settings Add in `./Settings.json`
            .add_source(config::File::with_name("./src/server/test/Default").required(true))
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
