use crate::{
    db::{Database, TransferRecord},
    types::{TransferEvent, TRANSFER_SIG, USDC_ADDRESS},
};
use eyre::Result;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::FullNodeComponents;
use tracing::{error, info, warn};

pub struct UsdcIndexer {
    db: Database,
}

impl UsdcIndexer {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Main loop — consumes ExEx notifications from the Reth node.
    pub async fn run<Node: FullNodeComponents>(
        self,
        mut ctx: ExExContext<Node>,
    ) -> Result<()> {
        info!("USDC Indexer ExEx started");

        while let Some(notification) = ctx.notifications.recv().await {
            let tip = match &notification {
                ExExNotification::ChainCommitted { new } => {
                    self.handle_committed(new)?;
                    new.tip().number
                }
                ExExNotification::ChainReorged { old, new } => {
                    self.handle_reorged(old, new)?;
                    new.tip().number
                }
                ExExNotification::ChainReverted { old } => {
                    self.handle_reverted(old)?;
                    // tip is the block *before* the reverted range
                    old.first().map(|(b, _)| b.number.saturating_sub(1)).unwrap_or(0)
                }
            };

            ctx.events.send(ExExEvent::FinishedHeight(tip))?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------

    fn handle_committed(&self, chain: &reth_execution_types::Chain) -> Result<()> {
        let records = Self::extract_records(chain)?;
        self.db.append_transfers(&records)?;
        if let Some(tip) = chain.tip() {
            self.db.set_last_block(tip.number)?;
            info!("Committed up to block {}", tip.number);
        }
        Ok(())
    }

    fn handle_reorged(
        &self,
        old: &reth_execution_types::Chain,
        new: &reth_execution_types::Chain,
    ) -> Result<()> {
        let fork_block = old
            .first()
            .map(|(b, _)| b.number.saturating_sub(1))
            .unwrap_or(0);

        warn!(
            "Reorg: reverting {} blocks to fork point {fork_block}, then applying {} new blocks",
            old.len(),
            new.len(),
        );

        // rollback_to_block prunes the transfer log and rebuilds balances.
        self.db.rollback_to_block(fork_block)?;

        // Append the new chain's events and patch balances forward.
        let records = Self::extract_records(new)?;
        self.db.append_transfers(&records)?;

        if let Some(tip) = new.tip() {
            self.db.set_last_block(tip.number)?;
        }
        Ok(())
    }

    fn handle_reverted(&self, old: &reth_execution_types::Chain) -> Result<()> {
        let fork_block = old
            .first()
            .map(|(b, _)| b.number.saturating_sub(1))
            .unwrap_or(0);

        warn!("Revert: rolling back to block {fork_block}");
        self.db.rollback_to_block(fork_block)?;
        self.db.set_last_block(fork_block)?;
        Ok(())
    }

    // -----------------------------------------------------------------------

    /// Collect all USDC Transfer events from `chain` into an ordered vec of
    /// `TransferRecord`s, preserving block order.
    fn extract_records(chain: &reth_execution_types::Chain) -> Result<Vec<TransferRecord>> {
        let mut records = Vec::new();

        for (block, receipts) in chain.blocks_and_receipts() {
            let block_number = block.number;

            for (receipt, _tx) in receipts.iter().zip(block.body.transactions.iter()) {
                if !receipt.success {
                    continue;
                }

                for log in &receipt.logs {
                    if log.address != USDC_ADDRESS {
                        continue;
                    }
                    let topics = log.topics();
                    if topics.first() != Some(&TRANSFER_SIG) {
                        continue;
                    }

                    let t = match TransferEvent::from_log(topics, &log.data.data, block_number) {
                        Ok(t) => t,
                        Err(e) => {
                            error!("Parse error block {block_number}: {e}");
                            continue;
                        }
                    };

                    records.push(TransferRecord {
                        block_number: t.block_number,
                        from_addr: t.from,
                        to_addr: t.to,
                        value: t.value,
                    });
                }
            }
        }

        Ok(records)
    }
}
