use anyhow::{anyhow, Result};
use futures_util::{stream::FuturesUnordered, StreamExt};
use network::plaintcp::CancelHandler;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender, UnboundedReceiver};
use log::*;

pub struct QuorumWaiter {
    threshold: usize,
    waiter_tx: UnboundedSender<Vec<CancelHandler>>,
}

impl QuorumWaiter {
    pub fn new(threshold: usize) -> Self {
        let (waiter_tx, waiter_rx) = unbounded_channel();

        // Start the waiter thread
        tokio::spawn(async move {
            Self::wait_remaining(waiter_rx)
        });

        Self { 
            threshold,
            waiter_tx,
        }
    }

    /// After receiving acks from `threshold` people, 
    /// we drive the remaining send(s) to completion using this thread
    async fn wait_remaining(
        mut rx: UnboundedReceiver<Vec<CancelHandler>>
    ) -> Result<()> 
    {
        let mut wait_stream = FuturesUnordered::new();
        loop {
            tokio::select! {
                handelrs_opt = rx.recv() => {
                    if let None = handelrs_opt {
                        break;
                    }
                    let handlers = handelrs_opt.unwrap();
                    for handler in handlers {
                        wait_stream.push(handler);
                    }
                },
                Some(Ok(ack)) = wait_stream.next() => {
                    debug!(
                        "Finished sending to another node with ack: {:?}", 
                        ack
                    );
                }
            }
        }
        Ok(())
    }

    /// Wait for `threshold` nodes to ack the message
    pub async fn wait(
        &mut self,
        handlers: Vec<CancelHandler>,
    ) -> Result<()> {
        assert!(
            handlers.len() >= self.threshold, 
            "Error: Cannot wait for {} acks with {} handlers",
            self.threshold,
            handlers.len(),
        );

        let mut wait_stream = FuturesUnordered::new();
        
        // Add all the handlers to the wait list
        for handler in handlers {
            wait_stream.push(handler);
        }

        // Wait for `threshold` number of them to finish successfully
        let mut success = 0usize;
        while success < self.threshold && !wait_stream.is_empty() {
            if let Some(Ok(_)) = wait_stream.next().await {
                success += 1;
            } 
        }

        // If we exited because we drove all the futures to completion, and we did not get self.threshold acks
        if wait_stream.is_empty() && success < self.threshold {
            return Err(anyhow!(
                "Did not return sufficient acks after driving all the senders: Expected: {}, Got {}",
                self.threshold,
                success
            ));
        }

        // If empty, we have nothing else to do
        if wait_stream.is_empty() {
            return Ok(());
        }

        // Collect all the remaining unfinished receives
        let remaining: Vec<_> = wait_stream
            .into_iter()
            .collect();
        
        // Dispatch remaining receives to the waiter thread
        self.waiter_tx
            .send(remaining)?;
        
        // Notify the caller
        return Ok(());
    }
}
