use crate::server::Config;
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    pub port: u16,
    pub consensus_config: Config,
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
                config::Environment::with_prefix("LETO_CLIENT")
                    .try_parsing(true)
                    .separator("_")
                    .list_separator(" "),
            )
            .build()?;
        conf.try_deserialize().map_err(|e| anyhow::Error::new(e))
    }
}
