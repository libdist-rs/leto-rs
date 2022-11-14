use std::time::Duration;

use crate::server::{Id, Party, Round, Server, Settings, StorageConfig};
use anyhow::{anyhow, Result};
use fnv::FnvHashMap;

fn dummy_ids(num_nodes: usize) -> Vec<Id> {
    let mut ids = Vec::with_capacity(num_nodes);
    for i in 0..num_nodes {
        ids.push(i.into());
    }
    ids
}

fn dummy_settings(num_nodes: usize) -> Settings {
    // Returns dummy settings
    let ids = dummy_ids(num_nodes);
    let mempool_config = mempool::Config::<Round>::default();
    let storage_config = StorageConfig {
        base: format!("src/server/test"),
        prefix: format!("db"),
    };
    let mut parties = FnvHashMap::default();
    for i in 0..num_nodes {
        let id = ids[i];
        parties.insert(
            id.clone(),
            Party {
                id: id,
                consensus_address: format!("127.0.0.1"),
                consensus_port: 6000 + (i as u16),
                mempool_address: format!("127.0.0.1"),
                mempool_port: 7000 + (i as u16),
                client_port: 8000 + (i as u16),
            },
        );
    }
    Settings {
        mempool_config,
        consensus_config: crate::server::Config { parties },
        storage: storage_config,
    }
}

const DEFAULT_CONFIG_FILE_LOCATION: &'static str = "./src/server/test/Default";

#[tokio::test]
async fn test_one() -> Result<()> {
    let settings = Settings::new(DEFAULT_CONFIG_FILE_LOCATION.to_string())?;
    let ids = dummy_ids(settings.consensus_config.num_nodes());
    let exit_tx = Server::spawn(ids[0], ids, settings)?;
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let res = exit_tx.send(());
    res.map_err(|_| anyhow!("Server did not successfully terminate"))
}

#[tokio::test]
async fn test_settings() -> Result<()> {
    let _settings = Settings::new(DEFAULT_CONFIG_FILE_LOCATION.to_string())?;
    Ok(())
}
