use clarity::Address as EthAddress;
use clarity::Uint256;
use gravity_utils::types::event_signatures::SENT_TO_COSMOS_EVENT_SIG;
use gravity_utils::types::EthereumEvent;
use gravity_utils::types::SendToCosmosEvent;
use web30::client::Web3;

/// This is roughly the maximum number of blocks a reasonable Ethereum node
/// can search in a single request before it starts timing out or behaving badly
pub const BLOCKS_TO_SEARCH: u128 = 5_000u128;

pub async fn check_for_events(
    web3: &Web3,
    gravity_contract_address: EthAddress,
    starting_block: Uint256,
    ending_block: Uint256,
) {
    let mut current_block = ending_block;
    while current_block > starting_block {
        let end_search = if current_block.clone() < BLOCKS_TO_SEARCH.into() {
            0u8.into()
        } else {
            current_block.clone() - BLOCKS_TO_SEARCH.into()
        };
        let deposits = web3
            .check_for_events(
                end_search.clone(),
                Some(current_block),
                vec![gravity_contract_address],
                vec![SENT_TO_COSMOS_EVENT_SIG],
            )
            .await
            .unwrap();

        let deposits = SendToCosmosEvent::from_logs(&deposits).unwrap();
        for d in deposits {
            println!(
                "Deposit from ETH by {} to {} for {}{}",
                d.sender, d.destination, d.amount, d.erc20
            )
        }
        current_block = end_search;
    }
}
