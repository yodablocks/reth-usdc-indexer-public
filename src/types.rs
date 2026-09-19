use alloy_primitives::{address, Address, B256, U256};

/// USDC contract address on Ethereum Mainnet
pub const USDC_ADDRESS: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

/// keccak256("Transfer(address,address,uint256)")
pub const TRANSFER_SIG: B256 = B256::new([
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b,
    0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16,
    0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
]);

#[derive(Debug, Clone)]
pub struct TransferEvent {
    pub from: Address,
    pub to: Address,
    pub value: U256,
    pub block_number: u64,
}

impl TransferEvent {
    /// Parse a Transfer event from log topics and data.
    /// Expects: topics = [sig, from, to], data = abi-encoded uint256 value.
    pub fn from_log(
        topics: &[B256],
        data: &[u8],
        block_number: u64,
    ) -> eyre::Result<Self> {
        eyre::ensure!(topics.len() == 3, "Transfer event must have 3 topics");
        eyre::ensure!(data.len() == 32, "Transfer event data must be 32 bytes");

        let from = Address::from_word(topics[1]);
        let to = Address::from_word(topics[2]);
        let value = U256::from_be_slice(data);

        Ok(Self { from, to, value, block_number })
    }
}
