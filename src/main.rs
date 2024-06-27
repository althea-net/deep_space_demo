use clap::Parser;
use cosmos_sdk_proto_althea::{
    cosmos::{
        bank::v1beta1::MsgSend,
        distribution::v1beta1::MsgWithdrawDelegatorReward,
        staking::v1beta1::{MsgDelegate, MsgUndelegate},
        tx::v1beta1::{TxBody, TxRaw},
    },
    ibc::{applications::transfer::v1::MsgTransfer, core::channel::v1::MsgRecvPacket},
};
use deep_space::address::Address;
use deep_space::{
    client::{types::LatestBlock, Contact},
    utils::decode_any,
};
use futures::future::join_all;
use gravity_proto::gravity::{MsgSendToCosmosClaim, MsgSendToEth};
use prost_types::Any;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
    vec,
};

const TIMEOUT: Duration = Duration::from_secs(60);

/// finds earliest available block using binary search, keep in mind this cosmos
/// node will not have history from chain halt upgrades and could be state synced
/// and missing history before the state sync
/// Iterative implementation due to the limitations of async recursion in rust.
async fn get_earliest_block(contact: &Contact, mut start: u64, mut end: u64) -> u64 {
    while start <= end {
        let mid = start + (end - start) / 2;
        let mid_block = contact.get_block(mid).await;
        if let Ok(Some(_)) = mid_block {
            end = mid - 1;
        } else {
            start = mid + 1;
        }
    }
    // off by one error correction fix bounds logic up top
    start + 1
}

/// Searches a segment of blocks for transactions to or from a target address
/// returns a Hashmap of transactions indexed by block height
async fn search(
    contact: &Contact,
    target_address: Address,
    start: u64,
    end: u64,
) -> HashMap<u64, Vec<MessageWrapper>> {
    let blocks = contact.get_block_range(start, end).await.unwrap();
    let mut txs = HashMap::new();

    let blocks_len = blocks.len() as u64;
    for block in blocks {
        let block = block.unwrap();
        let block_num = block.header.unwrap().height as u64;
        for tx in block.data.unwrap().txs {
            let raw_tx_any = prost_types::Any {
                type_url: "/cosmos.tx.v1beta1.TxRaw".to_string(),
                value: tx,
            };
            let tx_raw: TxRaw = decode_any(raw_tx_any).unwrap();
            let _tx_hash = sha256::digest(&tx_raw.body_bytes);
            let body_any = prost_types::Any {
                type_url: "/cosmos.tx.v1beta1.TxBody".to_string(),
                value: tx_raw.body_bytes,
            };
            let tx_body: TxBody = decode_any(body_any).unwrap();
            for message in tx_body.messages {
                match message.type_url.as_str() {
                    "/cosmos.bank.v1beta1.MsgSend" => {
                        let send = decode_msg_send(message).unwrap();
                        let source_address: Address = send.from_address.parse().unwrap();
                        let destination_address: Address = send.to_address.parse().unwrap();
                        if source_address == target_address || destination_address == target_address
                        {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::Send(send));
                        }
                    }
                    "/cosmos.distribution.v1beta1.MsgWithdrawDelegatorReward" => {
                        let reward = decode_msg_withdraw_delegator_reward(message).unwrap();
                        let delegator_address: Address = reward.delegator_address.parse().unwrap();
                        if delegator_address == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::Reward(reward));
                        }
                    }
                    "/cosmos.staking.v1beta1.MsgDelegate" => {
                        let delegate = decode_msg_delegate(message).unwrap();
                        let delegator_address: Address =
                            delegate.delegator_address.parse().unwrap();
                        if delegator_address == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::Delegate(delegate));
                        }
                    }
                    "/cosmos.staking.v1beta1.MsgUndelegate" => {
                        let undelegate = decode_msg_undelegate(message).unwrap();
                        let delegator_address: Address =
                            undelegate.delegator_address.parse().unwrap();
                        if delegator_address == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::UnDelegate(undelegate));
                        }
                    }
                    "/ibc.applications.transfer.v1.MsgTransfer" => {
                        let transfer = decode_msg_transfer(message).unwrap();
                        let sender: Address = transfer.sender.parse().unwrap();
                        let receiver: Address = transfer.receiver.parse().unwrap();
                        if sender == target_address || receiver == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::Transfer(transfer));
                        }
                    }
                    "/ibc.core.channel.v1.MsgRecvPacket" => {
                        let recv_packet = decode_msg_recv_packet(message).unwrap();
                        //  TODO need to figure out how to decode this
                        let txs = txs.entry(block_num).or_insert_with(Vec::new);
                        txs.push(MessageWrapper::RecvPacket(recv_packet));
                    }
                    "/gravity.v1.MsgSendToCosmosClaim" => {
                        let send_to_cosmos_claim =
                            decode_msg_send_to_cosmos_claim(message).unwrap();
                        let destination_address: Address =
                            send_to_cosmos_claim.cosmos_receiver.parse().unwrap();
                        if destination_address == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::SendToCosmosClaim(send_to_cosmos_claim));
                        }
                    }
                    "/gravity.v1.MsgSendToEth" => {
                        let send_to_eth = decode_msg_send_to_eth(message).unwrap();
                        let source_address: Address = send_to_eth.sender.parse().unwrap();
                        if source_address == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::SendToEth(send_to_eth));
                        }
                    }
                    _ => {
                        //println!("unknown message type {}", message.type_url);
                    }
                }
            }
        }
    }
    print!(
        "Got batch of {} blocks, {} contain target messages \n",
        blocks_len,
        txs.len()
    );
    txs
}

/// Wrapper for messages that we expect and will decode
enum MessageWrapper {
    Send(MsgSend),
    Reward(MsgWithdrawDelegatorReward),
    Delegate(MsgDelegate),
    UnDelegate(MsgUndelegate),
    Transfer(MsgTransfer),
    RecvPacket(MsgRecvPacket),
    SendToCosmosClaim(MsgSendToCosmosClaim),
    SendToEth(MsgSendToEth),
}

fn decode_msg_send(message: Any) -> Option<MsgSend> {
    let send_any = prost_types::Any {
        type_url: "/cosmos.bank.v1beta1.MsgSend".to_string(),
        value: message.value,
    };
    let send: Result<MsgSend, _> = decode_any(send_any);
    match send {
        Ok(send) => Some(send),
        Err(_) => None,
    }
}

fn decode_msg_withdraw_delegator_reward(message: Any) -> Option<MsgWithdrawDelegatorReward> {
    let reward_any = prost_types::Any {
        type_url: "/cosmos.distribution.v1beta1.MsgWithdrawDelegatorReward".to_string(),
        value: message.value,
    };
    let reward: Result<MsgWithdrawDelegatorReward, _> = decode_any(reward_any);
    match reward {
        Ok(reward) => Some(reward),
        Err(_) => None,
    }
}

fn decode_msg_delegate(message: Any) -> Option<MsgDelegate> {
    let delegate_any = prost_types::Any {
        type_url: "/cosmos.staking.v1beta1.MsgDelegate".to_string(),
        value: message.value,
    };
    let delegate: Result<MsgDelegate, _> = decode_any(delegate_any);
    match delegate {
        Ok(delegate) => Some(delegate),
        Err(_) => None,
    }
}

fn decode_msg_undelegate(message: Any) -> Option<MsgUndelegate> {
    let undelegate_any = prost_types::Any {
        type_url: "/cosmos.staking.v1beta1.MsgUndelegate".to_string(),
        value: message.value,
    };
    let undelegate: Result<MsgUndelegate, _> = decode_any(undelegate_any);
    match undelegate {
        Ok(undelegate) => Some(undelegate),
        Err(_) => None,
    }
}

fn decode_msg_transfer(message: Any) -> Option<MsgTransfer> {
    let transfer_any = prost_types::Any {
        type_url: "/ibc.applications.transfer.v1.MsgTransfer".to_string(),
        value: message.value,
    };
    let transfer: Result<MsgTransfer, _> = decode_any(transfer_any);
    match transfer {
        Ok(transfer) => Some(transfer),
        Err(_) => None,
    }
}

fn decode_msg_recv_packet(message: Any) -> Option<MsgRecvPacket> {
    let recv_packet_any = prost_types::Any {
        type_url: "/ibc.core.channel.v1.MsgRecvPacket".to_string(),
        value: message.value,
    };
    let recv_packet: Result<MsgRecvPacket, _> = decode_any(recv_packet_any);
    match recv_packet {
        Ok(recv_packet) => Some(recv_packet),
        Err(_) => None,
    }
}

fn decode_msg_send_to_cosmos_claim(message: Any) -> Option<MsgSendToCosmosClaim> {
    let send_to_cosmos_claim_any = prost_types::Any {
        type_url: "/gravity.v1.MsgSendToCosmosClaim".to_string(),
        value: message.value,
    };
    let send_to_cosmos_claim: Result<MsgSendToCosmosClaim, _> =
        decode_any(send_to_cosmos_claim_any);
    match send_to_cosmos_claim {
        Ok(send_to_cosmos_claim) => Some(send_to_cosmos_claim),
        Err(_) => None,
    }
}

fn decode_msg_send_to_eth(message: Any) -> Option<MsgSendToEth> {
    let send_to_eth_any = prost_types::Any {
        type_url: "/gravity.v1.MsgSendToEth".to_string(),
        value: message.value,
    };
    let send_to_eth: Result<MsgSendToEth, _> = decode_any(send_to_eth_any);
    match send_to_eth {
        Ok(send_to_eth) => Some(send_to_eth),
        Err(_) => None,
    }
}

fn merge_search_results(
    search_results: Vec<HashMap<u64, Vec<MessageWrapper>>>,
) -> HashMap<u64, Vec<MessageWrapper>> {
    let mut merged = HashMap::new();
    for search_result in search_results {
        for (block_num, messages) in search_result {
            let merged_messages = merged.entry(block_num).or_insert_with(Vec::new);
            merged_messages.extend(messages);
        }
    }
    merged
}

const DEFAULT_RPC: &str = "https://gravitychain.io:9090";

/// Command line arguments
#[derive(Parser)]
#[clap(version = env!("CARGO_PKG_VERSION"), author = "Justin Kilpatrick <justin@althea.net>")]
struct Opts {
    /// Target account to generate a csv for
    #[arg(short, long)]
    target_account: Address,

    /// rpc url to use
    #[arg(short, long, default_value = DEFAULT_RPC)]
    rpc: String,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = Opts::parse();
    let prefix = args.target_account.get_prefix();

    let contact = Contact::new(&args.rpc, TIMEOUT, &prefix).expect("invalid url");

    let status = contact
        .get_latest_block()
        .await
        .expect("Failed to get chain status, grpc error");

    // get the latest block this node has
    let latest_block = match status {
        LatestBlock::Latest { block } | LatestBlock::Syncing { block } => {
            block.header.unwrap().height as u64
        }
        _ => panic!("Node is not synced or not running"),
    };

    // now we find the earliest block this node has via binary search, we could just read it from
    // the error message you get when requesting an earlier block, but this was more fun
    let earliest_block = get_earliest_block(&contact, 0, latest_block).await;
    println!(
        "This node has {} blocks to download, starting clock now",
        latest_block - earliest_block
    );
    let start = Instant::now();

    const BATCH_SIZE: u64 = 50;
    const EXECUTE_SIZE: usize = 50;
    let mut pos = earliest_block;
    let mut futures = Vec::new();
    while pos < latest_block {
        let start = pos;
        let end = if latest_block - pos > BATCH_SIZE {
            pos += BATCH_SIZE;
            pos
        } else {
            pos = latest_block;
            latest_block
        };
        let fut = search(&contact, args.target_account, start, end);
        futures.push(fut);
    }

    let mut futures = futures.into_iter();

    // we merge all the messages into this one hashmap
    let mut merged = HashMap::new();
    let mut buf = Vec::new();
    while let Some(fut) = futures.next() {
        if buf.len() < EXECUTE_SIZE {
            buf.push(fut);
        } else {
            let res = join_all(buf).await;
            let batch_merged = merge_search_results(res);
            merged = merge_search_results(vec![merged, batch_merged]);
            println!(
                "Completed batch of {} blocks",
                BATCH_SIZE * EXECUTE_SIZE as u64
            );
            buf = Vec::new();
        }
    }
    let res = join_all(buf).await;

    // the final storage of all target messages we have found
    let final_merged = merge_search_results(vec![merged, merge_search_results(res)]);
    
    // now we make a csv of the resulting messages


    let elapsed = start.elapsed();
    println!(
        "Completed transaction scan and dump elapsed time: {:?}",
        elapsed
    );
}

/// Takes the combined user messages, creates a csv file with the following columns
fn make_csv(input: HashMap<u64, Vec<MessageWrapper>>) {

}