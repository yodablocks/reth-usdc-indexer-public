# reth-usdc-indexer

A real-time USDC transfer indexer that runs **inside** a [Reth](https://github.com/paradigmxyz/reth) node as an [ExEx](https://reth.rs/developers/exex/exex) (Execution Extension).

No RPC polling, no external node calls, no `eth_getLogs` pagination. The indexer receives committed blocks directly from the execution pipeline, filters USDC `Transfer` logs, and maintains a queryable balance table that stays correct across chain reorganizations.

## Why this exists

The usual way to index an ERC-20 is to poll an RPC endpoint for logs. That approach inherits three problems: network round-trips bound your latency, you pay per request, and reorg handling is left as an exercise for the reader (most implementations silently corrupt their state when the chain reorganizes).

Running as an ExEx removes all three. The node hands you the chain state it already computed.

## Design

```
Reth node ──► ExExNotification ──► UsdcIndexer ──► SQLite
                                        │
              ChainCommitted  ──────────┤ append events, patch balances
              ChainReorged    ──────────┤ rollback to fork, replay new chain
              ChainReverted   ──────────┘ rollback to fork
```

Two tables, with a deliberate split:

- **`transfers`** is an append-only event log. It is the source of truth.
- **`balances`** is a materialized view derived from it, carrying `last_updated_block`.

Rollback is therefore always exact rather than approximate: delete events after the fork point, then rebuild the affected balances by replaying the survivors. There is no guesswork about what a balance "was" at a prior height, because the event log can always reconstruct it.

The ExEx acknowledges each processed height with `ExExEvent::FinishedHeight`, which is what lets Reth prune data it no longer needs to retain.

## Reorg handling

All three notification variants are handled, which is the part most indexer examples omit:

| Notification | Action |
|---|---|
| `ChainCommitted` | Append new transfers, patch balances forward |
| `ChainReorged` | Roll back to fork point, then apply the new chain |
| `ChainReverted` | Roll back to fork point |

Correctness is covered by tests, including the cases that are easy to get wrong:

- `reorg_restores_prior_balance` — a balance returns to its pre-fork value
- `reorg_of_address_with_pre_fork_and_post_fork_activity` — an address active on *both* sides of the fork is rebuilt correctly, not double-counted or zeroed
- `full_reorg_to_genesis_clears_all` — rolling back everything leaves clean state

```bash
cargo test
```

Tests and the benchmark build without Reth: the storage layer is independent of it, so a clone runs in seconds rather than waiting on the full node tree.

## Measured query latency

`get_balance` is an indexed point lookup against the materialized table on a prepared statement, so reads do not touch the event log.

Benchmark: 50,000 addresses, 550,000 transfer events, 66 MB database, 20,000 randomised lookups after cache warm-up.

| | latency |
|---|---|
| mean | 2.2 µs |
| p50 | 2.2 µs |
| p95 | 2.5 µs |
| p99 | 2.8 µs |
| max | 7.1 µs |

Measured on an Apple M4, 32 GB RAM, rustc 1.96.0, `--release`. Reproduce with:

```bash
cargo run --release --bin bench-balance
```

This measures the **query path only**. Indexing throughput is bounded by Reth's block execution, not by this crate, so it is not what these numbers describe.

## Running

Requires a synced Reth node; the ExEx installs into it. The Reth dependency
is behind the `exex` feature, so building the node binary is opt-in:

```bash
cargo build --release --features exex

# Database path is configurable; defaults to ./indexer.db
INDEXER_DB=/var/lib/usdc/indexer.db \
  ./target/release/reth-usdc-indexer node \
    --chain mainnet \
    --datadir /var/lib/reth
```

Standard Reth CLI flags all apply, since the binary wraps `reth::cli::Cli`.

> **Note on the Reth pin.** This is pinned to Reth `v1.9.0` (February 2026),
> which is the version it was written and run against. Building with
> `--features exex` currently fails against today's crates.io, because Reth
> v1.9.0 requires `alloy-evm ^0.23`, and `alloy-evm 0.23.3` no longer compiles
> against the `alloy-rpc-types-eth` version that same tree resolves. That is
> upstream version drift, not a change in this code. Reth is on v2.x now;
> moving the pin forward would mean reworking the ExEx integration against a
> changed API. The storage layer, reorg logic, and tests are unaffected and
> build on their own.

## Scope and limitations

Deliberately narrow, and worth stating plainly:

- **One token.** USDC on Ethereum mainnet, hardcoded in `types.rs`. Widening it to arbitrary ERC-20s means a contract-address filter set, not an architectural change.
- **SQLite.** Well matched to single-node point lookups. A high-concurrency serving path would want Postgres or an embedded KV store.
- **Balances are transfer-derived.** They reflect `Transfer` events, which is exactly right for USDC but would miss rebasing or fee-on-transfer tokens where balances move without an event.
- **No API layer.** It maintains a database; serving it over HTTP is left to the caller.

## Layout

```
src/types.rs   USDC address, Transfer signature, log decoding
src/db.rs      Schema, append path, reorg rollback, balance queries, tests
src/exex.rs    ExEx notification loop
src/main.rs    Node builder and ExEx installation
benches/       Latency benchmark
```

Pinned to Reth `v1.9.0`.

## License

MIT
