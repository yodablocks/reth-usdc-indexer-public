use alloy_primitives::{Address, U256};
use eyre::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// A single ERC-20 Transfer event to be appended to the event log.
#[derive(Debug, Clone)]
pub struct TransferRecord {
    pub block_number: u64,
    pub from_addr: Address,
    pub to_addr: Address,
    pub value: U256,
}

/// Thread-safe SQLite database.
///
/// # Schema design
/// `transfers` is the source of truth — an append-only event log.
/// `balances` is a materialized view recomputed from `transfers` whenever a
/// reorg occurs.  This means rollback is always exact: delete events after the
/// fork point, then rebuild balances by replaying the survivors.
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    pub fn new(path: &str) -> Result<Self> {
        let conn = Connection::open(path).context("Failed to open SQLite database")?;

        // WAL mode: allows concurrent readers while the ExEx is writing.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        conn.execute_batch("
            -- Append-only event log: one row per Transfer event seen on-chain.
            CREATE TABLE IF NOT EXISTS transfers (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                block_number INTEGER NOT NULL,
                from_addr    TEXT    NOT NULL,
                to_addr      TEXT    NOT NULL,
                value        TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_transfers_block
                ON transfers(block_number);

            -- Materialized balance view, rebuilt after every reorg.
            CREATE TABLE IF NOT EXISTS balances (
                address             TEXT    PRIMARY KEY,
                balance             TEXT    NOT NULL,
                last_updated_block  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_balances_block
                ON balances(last_updated_block);

            -- Singleton: highest fully-processed block.
            CREATE TABLE IF NOT EXISTS sync_state (
                id         INTEGER PRIMARY KEY CHECK (id = 1),
                last_block INTEGER NOT NULL
            );
        ").context("Failed to initialize schema")?;

        info!("Database schema ready");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Normal path: append events, then patch the materialized balances table.
    // -----------------------------------------------------------------------

    /// Append transfer events and update the materialized `balances` table
    /// in a single atomic transaction.
    pub fn append_transfers(&self, records: &[TransferRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        // 1. Insert events into the log.
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO transfers (block_number, from_addr, to_addr, value)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for r in records {
                ins.execute(params![
                    r.block_number as i64,
                    format!("{:#x}", r.from_addr),
                    format!("{:#x}", r.to_addr),
                    r.value.to_string(),
                ])?;
                debug!(
                    "transfer {:#x} → {:#x}  {} @ {}",
                    r.from_addr, r.to_addr, r.value, r.block_number
                );
            }
        }

        // 2. Patch `balances` incrementally for the addresses touched by these
        //    records.  Because the records are already ordered by block, we can
        //    apply the deltas directly without a full replay.
        Self::patch_balances_in_tx(&tx, records)?;

        tx.commit()?;
        Ok(())
    }

    /// Apply balance deltas for a slice of transfers inside an existing
    /// transaction.  Called both from `append_transfers` and after a reorg.
    fn patch_balances_in_tx(
        tx: &rusqlite::Transaction<'_>,
        records: &[TransferRecord],
    ) -> Result<()> {
        let mut upsert = tx.prepare_cached(
            "INSERT INTO balances (address, balance, last_updated_block)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(address) DO UPDATE SET
                 balance            = excluded.balance,
                 last_updated_block = excluded.last_updated_block",
        )?;
        let mut lookup = tx.prepare_cached(
            "SELECT balance FROM balances WHERE address = ?1",
        )?;

        for r in records {
            // Sender
            let sender_key = format!("{:#x}", r.from_addr);
            let sender_bal: U256 = lookup
                .query_row(params![&sender_key], |row| row.get::<_, String>(0))
                .optional()?
                .as_deref()
                .map(|s| s.parse::<U256>().unwrap_or(U256::ZERO))
                .unwrap_or(U256::ZERO);
            upsert.execute(params![
                &sender_key,
                sender_bal.saturating_sub(r.value).to_string(),
                r.block_number as i64,
            ])?;

            // Receiver
            let recv_key = format!("{:#x}", r.to_addr);
            let recv_bal: U256 = lookup
                .query_row(params![&recv_key], |row| row.get::<_, String>(0))
                .optional()?
                .as_deref()
                .map(|s| s.parse::<U256>().unwrap_or(U256::ZERO))
                .unwrap_or(U256::ZERO);
            upsert.execute(params![
                &recv_key,
                recv_bal.saturating_add(r.value).to_string(),
                r.block_number as i64,
            ])?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Reorg path: delete events after fork point, rebuild balances from log.
    // -----------------------------------------------------------------------

    /// Roll back to `fork_block` (inclusive).
    ///
    /// 1. Deletes all transfer events with `block_number > fork_block`.
    /// 2. Drops and rebuilds the entire `balances` table by replaying the
    ///    surviving events.  The replay is a single SQL pass — O(n) in the
    ///    number of surviving transfers.
    pub fn rollback_to_block(&self, fork_block: u64) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let deleted = tx.execute(
            "DELETE FROM transfers WHERE block_number > ?1",
            params![fork_block as i64],
        )?;
        warn!("Reorg: pruned {deleted} transfer events, replaying from log up to block {fork_block}");

        // Rebuild balances from the surviving event log.
        // CAST to REAL for arithmetic because SQLite integers are 64-bit signed
        // and USDC values fit comfortably (total supply ~4×10¹³ µUSDC < 2⁶³).
        // We keep the balance column as TEXT for storage but compute in SQL.
        tx.execute_batch("
            DELETE FROM balances;

            INSERT INTO balances (address, balance, last_updated_block)
            SELECT
                address,
                CAST(SUM(delta) AS TEXT),
                MAX(block_number)
            FROM (
                -- Each 'to' entry is a credit (+value)
                SELECT to_addr   AS address,  CAST(value AS INTEGER) AS delta, block_number FROM transfers
                UNION ALL
                -- Each 'from' entry is a debit (-value)
                SELECT from_addr AS address, -CAST(value AS INTEGER) AS delta, block_number FROM transfers
            )
            GROUP BY address
            HAVING SUM(delta) > 0;
        ")?;

        tx.commit()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// Current balance for a single address.
    pub fn get_balance(&self, address: &Address) -> Result<Option<(U256, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT balance, last_updated_block FROM balances WHERE address = ?1",
        )?;

        let result = stmt
            .query_row(params![format!("{:#x}", address)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .optional()?;

        match result {
            None => Ok(None),
            Some((bal_str, block)) => {
                let balance = bal_str
                    .parse::<U256>()
                    .map_err(|e| eyre::eyre!("Invalid balance in DB: {e}"))?;
                Ok(Some((balance, block as u64)))
            }
        }
    }

    pub fn set_last_block(&self, block_number: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sync_state (id, last_block) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET last_block = ?1",
            params![block_number as i64],
        )?;
        Ok(())
    }

    pub fn get_last_block(&self) -> Result<Option<u64>> {
        let conn = self.conn.lock().unwrap();
        let result = conn
            .query_row("SELECT last_block FROM sync_state WHERE id = 1", [], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?;
        Ok(result.map(|b| b as u64))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn test_db() -> Database {
        Database::new(":memory:").unwrap()
    }

    fn addr(n: u8) -> Address {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        Address::from(bytes)
    }

    fn transfer(from: Address, to: Address, value: u64, block: u64) -> TransferRecord {
        TransferRecord {
            block_number: block,
            from_addr: from,
            to_addr: to,
            value: U256::from(value),
        }
    }

    // Mint: from == zero address
    fn mint(to: Address, value: u64, block: u64) -> TransferRecord {
        transfer(Address::ZERO, to, value, block)
    }

    #[test]
    fn basic_mint_and_transfer() {
        let db = test_db();
        let alice = addr(1);
        let bob = addr(2);

        // Block 100: mint 1000 to alice
        db.append_transfers(&[mint(alice, 1000, 100)]).unwrap();
        let (bal, _) = db.get_balance(&alice).unwrap().unwrap();
        assert_eq!(bal, U256::from(1000u64));

        // Block 101: alice sends 400 to bob
        db.append_transfers(&[transfer(alice, bob, 400, 101)]).unwrap();
        let (alice_bal, _) = db.get_balance(&alice).unwrap().unwrap();
        let (bob_bal, _) = db.get_balance(&bob).unwrap().unwrap();
        assert_eq!(alice_bal, U256::from(600u64));
        assert_eq!(bob_bal, U256::from(400u64));
    }

    #[test]
    fn reorg_restores_prior_balance() {
        let db = test_db();
        let alice = addr(1);
        let bob = addr(2);

        // Block 100: mint 1000 to alice (pre-fork, survives reorg)
        db.append_transfers(&[mint(alice, 1000, 100)]).unwrap();

        // Block 101: alice sends 400 to bob (will be reorged away)
        db.append_transfers(&[transfer(alice, bob, 400, 101)]).unwrap();

        // Verify state before reorg
        let (alice_bal, _) = db.get_balance(&alice).unwrap().unwrap();
        assert_eq!(alice_bal, U256::from(600u64));

        // Reorg: fork at block 100, discard block 101
        db.rollback_to_block(100).unwrap();

        // Alice should be back to 1000; bob should have no balance
        let (alice_bal, _) = db.get_balance(&alice).unwrap().unwrap();
        assert_eq!(alice_bal, U256::from(1000u64));
        assert!(db.get_balance(&bob).unwrap().is_none());
    }

    #[test]
    fn reorg_of_address_with_pre_fork_and_post_fork_activity() {
        let db = test_db();
        let alice = addr(1);
        let bob = addr(2);
        let carol = addr(3);

        // Block 100: mint 1000 to alice
        db.append_transfers(&[mint(alice, 1000, 100)]).unwrap();
        // Block 101: alice → bob 300  (pre-fork activity for alice AND bob)
        db.append_transfers(&[transfer(alice, bob, 300, 101)]).unwrap();
        // Block 102: alice → carol 200  (will be reorged away)
        db.append_transfers(&[transfer(alice, carol, 200, 102)]).unwrap();

        // Pre-reorg: alice=500, bob=300, carol=200
        assert_eq!(db.get_balance(&alice).unwrap().unwrap().0, U256::from(500u64));
        assert_eq!(db.get_balance(&carol).unwrap().unwrap().0, U256::from(200u64));

        // Reorg to block 101
        db.rollback_to_block(101).unwrap();

        // alice=700, bob=300, carol=gone
        assert_eq!(db.get_balance(&alice).unwrap().unwrap().0, U256::from(700u64));
        assert_eq!(db.get_balance(&bob).unwrap().unwrap().0, U256::from(300u64));
        assert!(db.get_balance(&carol).unwrap().is_none());
    }

    #[test]
    fn full_reorg_to_genesis_clears_all() {
        let db = test_db();
        let alice = addr(1);

        db.append_transfers(&[mint(alice, 500, 50)]).unwrap();
        db.rollback_to_block(0).unwrap();
        assert!(db.get_balance(&alice).unwrap().is_none());
    }
}
