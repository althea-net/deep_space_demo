use deep_space::client::types::AccountType;
use deep_space::client::PAGE;
use deep_space::error::CosmosGrpcError;
use deep_space::Coin;
use futures::future::{join3, join_all};
use gravity_proto::cosmos_sdk_proto::cosmos::bank::v1beta1::query_client::QueryClient as BankQueryClient;
use gravity_proto::cosmos_sdk_proto::cosmos::bank::v1beta1::QueryBalanceRequest;
use gravity_proto::cosmos_sdk_proto::cosmos::distribution::v1beta1::query_client::QueryClient as DistQueryClient;
use gravity_proto::cosmos_sdk_proto::cosmos::distribution::v1beta1::QueryDelegationTotalRewardsRequest;
use gravity_proto::cosmos_sdk_proto::cosmos::staking::v1beta1::query_client::QueryClient as StakingQueryClient;
use gravity_proto::cosmos_sdk_proto::cosmos::staking::v1beta1::QueryDelegatorDelegationsRequest;
use num256::Uint256;
use tonic::transport::channel::Channel;

use crate::GRAVITY_NODE_GRPC;

/// Dispatching utility function for building an array of joinable futures containing sets of batch requests
pub async fn get_balances_for_accounts(
    input: Vec<AccountType>,
    denom: String,
) -> Result<Vec<UserInfo>, CosmosGrpcError> {
    // handed tuned parameter for the ideal number of queryes per BankQueryClient
    const BATCH_SIZE: usize = 500;
    let mut index = 0;
    let mut futs = Vec::new();
    while index + BATCH_SIZE < input.len() - 1 {
        futs.push(batch_query_user_information(
            &input[index..index + BATCH_SIZE],
            denom.clone(),
        ));
        index += BATCH_SIZE;
    }
    futs.push(batch_query_user_information(&input[index..], denom.clone()));

    let executed_futures = join_all(futs).await;
    let mut balances = Vec::new();
    for b in executed_futures {
        balances.extend(b?);
    }
    Ok(balances)
}

/// Utility function for batching balance requests so that they occupy a single bankqueryclient which represents a connection
/// to the rpc server, opening connections is overhead intensive so we want to do a few thousand requests per client to really
/// make it worth our while
async fn batch_query_user_information(
    input: &[AccountType],
    denom: String,
) -> Result<Vec<UserInfo>, CosmosGrpcError> {
    let mut bankrpc = BankQueryClient::connect(GRAVITY_NODE_GRPC)
        .await?
        .accept_gzip();
    let mut distrpc = DistQueryClient::connect(GRAVITY_NODE_GRPC)
        .await?
        .accept_gzip();
    let mut stakingrpc = StakingQueryClient::connect(GRAVITY_NODE_GRPC)
        .await?
        .accept_gzip();

    let mut ret = Vec::new();
    for account in input {
        let res = merge_user_information(
            account.clone(),
            denom.clone(),
            &mut bankrpc,
            &mut distrpc,
            &mut stakingrpc,
        )
        .await?;
        ret.push(res);
    }
    Ok(ret)
}

/// utility function for keeping the Account and Balance info
/// in the same scope rather than zipping them on return
async fn merge_user_information(
    account: AccountType,
    denom: String,
    bankrpc: &mut BankQueryClient<Channel>,
    distrpc: &mut DistQueryClient<Channel>,
    stakingrpc: &mut StakingQueryClient<Channel>,
) -> Result<UserInfo, CosmosGrpcError> {
    // required because dec coins are multiplied by 1*10^18
    const ONE_ETH: u128 = 10u128.pow(18);

    let address = account.get_base_account().address;
    let balance_fut = bankrpc.balance(QueryBalanceRequest {
        address: address.to_string(),
        denom: denom.clone(),
    });
    let delegation_rewards_fut =
        distrpc.delegation_total_rewards(QueryDelegationTotalRewardsRequest {
            delegator_address: address.to_string(),
        });
    let total_delegated_fut = stakingrpc.delegator_delegations(QueryDelegatorDelegationsRequest {
        delegator_addr: address.to_string(),
        pagination: PAGE,
    });

    let (balance, delegation_rewards, total_delegated) =
        join3(balance_fut, delegation_rewards_fut, total_delegated_fut).await;

    let balance = balance?.into_inner();
    let delegation_rewards = delegation_rewards?.into_inner();
    let delegated = total_delegated?.into_inner();

    let balance = match balance.balance {
        Some(v) => {
            let v: Coin = v.into();
            v.amount
        }
        None => 0u8.into(),
    };

    let mut delegation_rewards_total: Uint256 = 0u8.into();
    for reward in delegation_rewards.total {
        if reward.denom == denom {
            delegation_rewards_total += reward.amount.parse().unwrap();
        }
        // you can total non-native token rewards in an else case here
    }
    delegation_rewards_total /= ONE_ETH.into();

    let mut total_delegated: Uint256 = 0u8.into();
    for delegated in delegated.delegation_responses {
        if let Some(b) = delegated.balance {
            let b: Coin = b.into();
            assert_eq!(b.denom, denom);
            total_delegated += b.amount
        }
    }

    Ok(UserInfo {
        account,
        balance,
        unclaimed_rewards: delegation_rewards_total,
        total_staked: total_delegated,
    })
}

pub struct UserInfo {
    pub account: AccountType,
    pub balance: Uint256,
    pub unclaimed_rewards: Uint256,
    pub total_staked: Uint256,
}
