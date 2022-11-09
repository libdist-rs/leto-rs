use std::time::Duration;

use crate::server::{Id, Server, Settings};
use anyhow::{anyhow, Result};
use tokio::sync::oneshot;

fn generate_ids(num_nodes: usize) -> Vec<Id> {
    let mut ids = Vec::with_capacity(num_nodes);
    for i in 0..num_nodes {
        ids.push(i.into());
    }
    ids
}

#[tokio::test]
async fn test_one() -> Result<()> {
    let (exit_tx, exit_rx) = oneshot::channel();
    let settings = Settings::new()?;
    let ids = generate_ids(settings.consensus_config.num_nodes);
    let _ = Server::spawn(ids[0], ids, settings, exit_rx);
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let res = exit_tx.send(());
    res.map_err(|_| anyhow!("Server did not successfully terminate"))
}

#[tokio::test]
async fn test_settings() -> Result<()> {
    let _settings = Settings::new()?;
    Ok(())
}
