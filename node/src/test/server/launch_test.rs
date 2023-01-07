use std::time::Duration;

use crate::{
    server::{BenchConfig, Party, Server, Settings, StorageConfig},
    Id, KeyConfig, Round, SimpleData, SimpleTx,
};
use anyhow::{anyhow, Result};
use crypto::Algorithm;
use fnv::FnvHashMap;
use tokio::{sync::mpsc::unbounded_channel, time::sleep};

fn dummy_ids(num_nodes: usize) -> Vec<Id> {
    let mut ids = Vec::with_capacity(num_nodes);
    for i in 0..num_nodes {
        ids.push(i);
    }
    ids
}

fn _dummy_settings(
    num_nodes: usize,
    _num_faults: usize,
) -> Settings {
    // Returns dummy settings
    let ids = dummy_ids(num_nodes);
    let mempool_config = mempool::Config::<Round>::default();
    let storage_config = StorageConfig {
        base: "src/server/test".to_string(),
        prefix: "db".to_string(),
    };
    let mut parties = FnvHashMap::default();
    for id in ids {
        parties.insert(
            id,
            Party {
                id,
                consensus_address: "127.0.0.1".to_string(), 
                consensus_port: 6000 + (id as u16),
                mempool_address: "127.0.0.1".to_string(),
                mempool_port: 7000 + (id as u16),
                client_port: 8000 + (id as u16),
            },
        );
    }
    Settings {
        mempool_config,
        committee_config: crate::server::Config { parties },
        storage: storage_config,
        bench_config: BenchConfig::default(),
    }
}

const DEFAULT_CONFIG_FILE_LOCATION: &str = "./examples/server";

#[tokio::test]
async fn test_one() -> Result<()> {
    let settings = Settings::new(DEFAULT_CONFIG_FILE_LOCATION)?;
    let num_nodes = settings.committee_config.num_nodes();

    // Build the ids
    let ids = dummy_ids(num_nodes);
    
    // Build the cryptosystem
    let crypto_system = KeyConfig::generate(
        Algorithm::ED25519, 
        num_nodes,
    )?;

    // Spawn the server
    const TEST_ID: Id = 0;
    let (commit_tx, _) = unbounded_channel();
    let exit_tx = Server::<SimpleTx<SimpleData>>::spawn(
        TEST_ID,
        ids,
        crypto_system[TEST_ID].clone(),
        settings,
        commit_tx,
    )?;

    // Sleep for some time until we can connect to all the servers
    sleep(Duration::from_millis(3_000)).await;

    // Try shutting down the server
    let res = exit_tx.send(());
    res.map_err(|_| anyhow!("Server did not successfully terminate"))
}

#[tokio::test]
async fn test_settings() -> Result<()> {
    let _settings = Settings::new(DEFAULT_CONFIG_FILE_LOCATION.to_string())?;
    Ok(())
}
