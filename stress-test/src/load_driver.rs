use crate::config::{build_client_settings, StressTestConfig};
use crate::smr::{SimpleData, SimpleTx};
use anyhow::Result;
use consensus::client::Stressor;
use tokio::sync::oneshot;

pub struct LoadDriver {
    stressor_exits: Vec<oneshot::Sender<()>>,
}

impl LoadDriver {
    pub fn start_load(
        config: &StressTestConfig,
        target_rate: u64,
    ) -> Result<Self> {
        let client_settings = build_client_settings(config, target_rate);
        let mut exits = Vec::new();

        for client_idx in 0..config.num_clients {
            // Client IDs start after server IDs to avoid collision
            let client_id = config.num_nodes + client_idx;
            let exit_tx = Stressor::<SimpleTx<SimpleData>>::spawn(
                client_id,
                client_settings.clone(),
            )?;
            exits.push(exit_tx);
        }

        Ok(Self {
            stressor_exits: exits,
        })
    }

    pub fn stop(self) {
        for sender in self.stressor_exits {
            let _ = sender.send(());
        }
    }
}
