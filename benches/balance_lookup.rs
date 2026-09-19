//! Point-lookup latency for `Database::get_balance`.
//!
//! Seeds a database with 50k addresses and 550k transfer events, then times
//! 20k randomised balance lookups and reports percentiles.
//!
//! Run with:  cargo run --release --bin bench-balance
//!
//! This measures the query path only (indexed SQLite read against the
//! materialized `balances` table). It deliberately excludes node sync and
//! block execution, which are bounded by Reth, not by this crate.

use alloy_primitives::{Address, U256};
use reth_usdc_indexer::{Database, TransferRecord};
use std::time::Instant;

const N_ADDRS: u64 = 50_000;
const N_TRANSFERS: u64 = 500_000;
const ITERS: u64 = 20_000;

fn addr(n: u64) -> Address {
    let mut b = [0u8; 20];
    b[12..20].copy_from_slice(&n.to_be_bytes());
    Address::from(b)
}

fn main() -> eyre::Result<()> {
    let path = std::env::var("BENCH_DB").unwrap_or_else(|_| "/tmp/bench-indexer.db".into());
    let _ = std::fs::remove_file(&path);
    let db = Database::new(&path)?;

    let t0 = Instant::now();
    let mut recs = Vec::with_capacity(10_000);

    // Mint to every address.
    for i in 0..N_ADDRS {
        recs.push(TransferRecord {
            block_number: 1 + i / 1000,
            from_addr: Address::ZERO,
            to_addr: addr(i),
            value: U256::from(1_000_000u64),
        });
        if recs.len() == 10_000 {
            db.append_transfers(&recs)?;
            recs.clear();
        }
    }
    if !recs.is_empty() {
        db.append_transfers(&recs)?;
        recs.clear();
    }

    // Spread transfers across the address space.
    for i in 0..N_TRANSFERS {
        recs.push(TransferRecord {
            block_number: 100 + i / 500,
            from_addr: addr(i % N_ADDRS),
            to_addr: addr((i * 7919 + 13) % N_ADDRS),
            value: U256::from(1u64),
        });
        if recs.len() == 10_000 {
            db.append_transfers(&recs)?;
            recs.clear();
        }
    }
    if !recs.is_empty() {
        db.append_transfers(&recs)?;
    }
    let seed = t0.elapsed().as_secs_f64();

    // Warm the page cache so we measure steady state, not first-touch I/O.
    for i in 0..1_000u64 {
        let _ = db.get_balance(&addr(i % N_ADDRS))?;
    }

    let mut samples = Vec::with_capacity(ITERS as usize);
    for i in 0..ITERS {
        let a = addr((i * 31_337) % N_ADDRS);
        let t = Instant::now();
        let _ = db.get_balance(&a)?;
        samples.push(t.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();

    let us = |v: u64| v as f64 / 1000.0;
    let pct = |p: f64| us(samples[((samples.len() as f64 - 1.0) * p) as usize]);
    let mean = us(samples.iter().sum::<u64>() / samples.len() as u64);
    let size_mb = std::fs::metadata(&path)?.len() as f64 / 1_048_576.0;

    println!("seed:     {} addresses, {} events in {:.1}s", N_ADDRS, N_TRANSFERS + N_ADDRS, seed);
    println!("db size:  {:.1} MB", size_mb);
    println!("lookups:  {}", ITERS);
    println!("  mean    {:.1} us", mean);
    println!("  p50     {:.1} us", pct(0.50));
    println!("  p95     {:.1} us", pct(0.95));
    println!("  p99     {:.1} us", pct(0.99));
    println!("  max     {:.1} us", us(*samples.last().unwrap()));
    Ok(())
}
