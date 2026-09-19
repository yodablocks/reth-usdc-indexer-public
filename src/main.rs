use eyre::Result;
use reth_node_ethereum::EthereumNode;
use reth_usdc_indexer::{Database, UsdcIndexer};
use tracing::info;

fn main() -> Result<()> {
    reth_tracing::init_tracing();

    let db_path = std::env::var("INDEXER_DB").unwrap_or_else(|_| "indexer.db".to_string());
    info!("Opening database at {db_path}");

    let db = Database::new(&db_path)?;
    let indexer = UsdcIndexer::new(db);

    reth::cli::Cli::parse_args().run(|builder, _args| async move {
        let handle = builder
            .node(EthereumNode::default())
            .install_exex("usdc-indexer", move |ctx| async move {
                Ok(indexer.run(ctx).await)
            })
            .launch()
            .await?;

        handle.wait_for_node_exit().await
    })
}
