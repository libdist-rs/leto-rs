use anyhow::{anyhow, Result};
use futures_util::{stream::FuturesUnordered, StreamExt};
use network::plaintcp::CancelHandler;

pub struct QuorumWaiter {
    threshold: usize,
}

impl QuorumWaiter {
    pub fn new(threshold: usize) -> Self {
        Self { threshold }
    }

    pub async fn wait(
        &self,
        handlers: Vec<CancelHandler>,
    ) -> Result<()> {
        let mut wait_stream = FuturesUnordered::new();
        for handler in handlers {
            wait_stream.push(handler);
        }
        let mut success = 0usize;
        while success < self.threshold {
            if let Some(Ok(_)) = wait_stream.next().await {
                success += 1;
            } else {
                break;
            }
        }
        if success == self.threshold {
            return Ok(());
        } else {
            return Err(anyhow!(
                "Did not return sufficient acks: Expected: {}, Got {}",
                self.threshold,
                success
            ));
        }
    }
}
