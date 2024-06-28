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
use csv::Writer;
use deep_space::{address::Address, Coin};
use deep_space::{
    client::{types::LatestBlock, Contact},
    utils::decode_any,
};
use futures::future::join_all;
use gravity_proto::gravity::{MsgSendToCosmosClaim, MsgSendToEth};
use prost_types::{Any, Timestamp};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
    vec,
};

const TIMEOUT: Duration = Duration::from_secs(60);

const MSG_SEND: &str = "/cosmos.bank.v1beta1.MsgSend";
const MSG_WITHDRAW_REWARD: &str = "/cosmos.distribution.v1beta1.MsgWithdrawDelegatorReward";
const MSG_DELEGATE: &str = "/cosmos.staking.v1beta1.MsgDelegate";
const MSG_UNDELEGATE: &str = "/cosmos.staking.v1beta1.MsgUndelegate";
const MSG_TRANSFER: &str = "/ibc.applications.transfer.v1.MsgTransfer";
const MSG_RECV_PACKET: &str = "/ibc.core.channel.v1.MsgRecvPacket";
const MSG_SEND_TO_COSMOS_CLAIM: &str = "/gravity.v1.MsgSendToCosmosClaim";
const MSG_SEND_TO_ETH: &str = "/gravity.v1.MsgSendToEth";

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

pub struct SearchReturn {
    messages: HashMap<u64, Vec<MessageWrapper>>,
    block_timestamps: HashMap<u64, Timestamp>,
}

/// Searches a segment of blocks for transactions to or from a target address
/// returns a Hashmap of transactions indexed by block height
async fn search(contact: &Contact, target_address: Address, start: u64, end: u64) -> SearchReturn {
    let blocks = contact.get_block_range(start, end).await.unwrap();
    let mut txs = HashMap::new();
    let mut block_timestamps = HashMap::new();

    let blocks_len = blocks.len() as u64;
    for block in blocks {
        let block = block.unwrap();
        let block_num = block.header.clone().unwrap().height as u64;
        let block_timestamp = block.header.unwrap().time.unwrap();
        block_timestamps.insert(block_num, block_timestamp);
        for tx in block.data.unwrap().txs {
            let raw_tx_any = prost_types::Any {
                type_url: "/cosmos.tx.v1beta1.TxRaw".to_string(),
                value: tx,
            };
            let tx_raw: TxRaw = decode_any(raw_tx_any).unwrap();
            let tx_hash = sha256::digest(&tx_raw.body_bytes);
            let body_any = prost_types::Any {
                type_url: "/cosmos.tx.v1beta1.TxBody".to_string(),
                value: tx_raw.body_bytes,
            };
            let tx_body: TxBody = decode_any(body_any).unwrap();
            for message in tx_body.messages {
                match message.type_url.as_str() {
                    MSG_SEND => {
                        let send = decode_msg_send(message);
                        let source_address: Address = send.from_address.parse().unwrap();
                        let destination_address: Address = send.to_address.parse().unwrap();
                        if source_address == target_address || destination_address == target_address
                        {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::Send(send));
                        }
                    }
                    MSG_WITHDRAW_REWARD => {
                        let reward = decode_msg_withdraw_delegator_reward(message);
                        let delegator_address: Address = reward.delegator_address.parse().unwrap();
                        if delegator_address == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            // the tx itself doesn't contain any info about what tokens we get as a reward, this requires on chain
                            // computation which is only displayed as a result in the logs, so we need to query the tx to get the logs
                            // and use those logs to compute what tokens where recieved.
                            let tx = contact
                                .get_tx_by_hash(tx_hash.clone())
                                .await
                                .unwrap()
                                .tx_response
                                .unwrap()
                                .logs;
                            let mut amounts = Vec::new();
                            for log in tx {
                                for event in log.events {
                                    if event.r#type == "coin_received"
                                        && event.attributes[0].key == "receiver"
                                        && event.attributes[0].value == target_address.to_string()
                                    {
                                        amounts.push(event.attributes[1].value.parse().unwrap());
                                    }
                                }
                            }
                            txs.push(MessageWrapper::Reward {
                                validator: reward.validator_address.parse().unwrap(),
                                delegator: delegator_address,
                                amounts,
                            })
                        }
                    }
                    MSG_DELEGATE => {
                        let delegate = decode_msg_delegate(message);
                        let delegator_address: Address =
                            delegate.delegator_address.parse().unwrap();
                        if delegator_address == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::Delegate(delegate));
                        }
                    }
                    MSG_UNDELEGATE => {
                        let undelegate = decode_msg_undelegate(message);
                        let delegator_address: Address =
                            undelegate.delegator_address.parse().unwrap();
                        if delegator_address == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::UnDelegate(undelegate));
                        }
                    }
                    MSG_TRANSFER => {
                        let transfer = decode_msg_transfer(message);
                        let sender: Address = transfer.sender.parse().unwrap();
                        let receiver: Address = transfer.receiver.parse().unwrap();
                        if sender == target_address || receiver == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::Transfer(transfer));
                        }
                    }
                    MSG_RECV_PACKET => {
                        // the tx itself doesn't contain any info about what tokens we have recieved via ibc or the sender
                        // this requires on chain computation which is only displayed as a result in the logs, so we need to query the tx to get the logs
                        // in this case we must first make sure the tx is a send packet to us, then we must check the logs for the amount
                        let tx = contact
                            .get_tx_by_hash(tx_hash.clone())
                            .await
                            .unwrap()
                            .tx_response
                            .unwrap()
                            .logs;
                        for log in tx {
                            for event in log.events {
                                if event.r#type == "fungible_token_packet"
                                // this check ensures some future ibc extensions don't break this parser by
                                // checking for specifically the type of packet we're looking at
                                    && event.attributes[0].key == "module"
                                    && event.attributes[0].value == "transfer"
                                    // make sure this is actually about our target address
                                    && event.attributes[2].key == "receiver"
                                    && event.attributes[2].value == target_address.to_string()
                                {
                                    let txs = txs.entry(block_num).or_insert_with(Vec::new);
                                    let mut amount = Coin {
                                        denom: event.attributes[3].key.clone(),
                                        amount: event.attributes[3].value.parse().unwrap(),
                                    };
                                    let sender = event.attributes[1].value.parse().unwrap();
                                    txs.push(MessageWrapper::RecvPacket {
                                        sender,
                                        reciver: target_address,
                                        amount: amount,
                                    });
                                }
                            }
                        }
                    }
                    MSG_SEND_TO_COSMOS_CLAIM => {
                        let send_to_cosmos_claim = decode_msg_send_to_cosmos_claim(message);
                        let destination_address: Address =
                            send_to_cosmos_claim.cosmos_receiver.parse().unwrap();
                        if destination_address == target_address {
                            let txs = txs.entry(block_num).or_insert_with(Vec::new);
                            txs.push(MessageWrapper::SendToCosmosClaim(send_to_cosmos_claim));
                        }
                    }
                    MSG_SEND_TO_ETH => {
                        let send_to_eth = decode_msg_send_to_eth(message);
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
    SearchReturn {
        messages: txs,
        block_timestamps,
    }
}

/// Wrapper for messages that we expect and will decode
enum MessageWrapper {
    Send(MsgSend),
    Reward {
        validator: Address,
        delegator: Address,
        amounts: Vec<Coin>,
    },
    Delegate(MsgDelegate),
    UnDelegate(MsgUndelegate),
    Transfer(MsgTransfer),
    RecvPacket {
        sender: Address,
        reciver: Address,
        amount: Coin,
    },
    SendToCosmosClaim(MsgSendToCosmosClaim),
    SendToEth(MsgSendToEth),
}

fn decode_msg_send(message: Any) -> MsgSend {
    let send_any = prost_types::Any {
        type_url: MSG_SEND.to_string(),
        value: message.value,
    };
    decode_any(send_any).unwrap()
}

fn decode_msg_withdraw_delegator_reward(message: Any) -> MsgWithdrawDelegatorReward {
    let reward_any = prost_types::Any {
        type_url: MSG_WITHDRAW_REWARD.to_string(),
        value: message.value,
    };
    decode_any(reward_any).unwrap()
}

fn decode_msg_delegate(message: Any) -> MsgDelegate {
    let delegate_any = prost_types::Any {
        type_url: MSG_DELEGATE.to_string(),
        value: message.value,
    };
    decode_any(delegate_any).unwrap()
}

fn decode_msg_undelegate(message: Any) -> MsgUndelegate {
    let undelegate_any = prost_types::Any {
        type_url: MSG_UNDELEGATE.to_string(),
        value: message.value,
    };
    decode_any(undelegate_any).unwrap()
}

fn decode_msg_transfer(message: Any) -> MsgTransfer {
    let transfer_any = prost_types::Any {
        type_url: MSG_TRANSFER.to_string(),
        value: message.value,
    };
    decode_any(transfer_any).unwrap()
}

fn decode_msg_send_to_cosmos_claim(message: Any) -> MsgSendToCosmosClaim {
    let send_to_cosmos_claim_any = prost_types::Any {
        type_url: MSG_SEND_TO_COSMOS_CLAIM.to_string(),
        value: message.value,
    };
    decode_any(send_to_cosmos_claim_any).unwrap()
}

fn decode_msg_send_to_eth(message: Any) -> MsgSendToEth {
    let send_to_eth_any = prost_types::Any {
        type_url: MSG_SEND_TO_ETH.to_string(),
        value: message.value,
    };
    decode_any(send_to_eth_any).unwrap()
}

/// Merges multiple `SearchReturn` instances into one.
/// This function combines the messages and block timestamps from multiple search results.
/// For `MsgSendToCosmosClaim` messages, it ensures there are no duplicates by retaining only one copy per `event_nonce`.
///
/// # Arguments
///
/// * `search_results` - A vector of `SearchReturn` instances to be merged.
///
/// # Returns
///
/// * A single `SearchReturn` instance with combined messages and block timestamps,
///   and deduplicated `MsgSendToCosmosClaim` messages.
fn merge_search_results(search_results: Vec<SearchReturn>) -> SearchReturn {
    let mut merged = HashMap::new();
    let mut block_timestamps = HashMap::new();
    let mut cosmos_claims: HashMap<u64, MsgSendToCosmosClaim> = HashMap::new();

    for search_result in search_results {
        for (block_num, messages) in search_result.messages {
            let merged_messages = merged.entry(block_num).or_insert_with(Vec::new);
            for message in messages {
                if let MessageWrapper::SendToCosmosClaim(claim) = &message {
                    if let Some(existing_claim) = cosmos_claims.get(&claim.event_nonce) {
                        if existing_claim != claim {
                            merged_messages.push(message);
                        }
                    } else {
                        cosmos_claims.insert(claim.event_nonce, claim.clone());
                        merged_messages.push(message);
                    }
                } else {
                    merged_messages.push(message);
                }
            }
        }
        block_timestamps.extend(search_result.block_timestamps);
    }

    SearchReturn {
        messages: merged,
        block_timestamps,
    }
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

    const BATCH_SIZE: u64 = 1_000;
    const EXECUTE_SIZE: usize = 250;
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
    let mut merged = SearchReturn {
        messages: HashMap::new(),
        block_timestamps: HashMap::new(),
    };
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
    make_csv(final_merged);

    let elapsed = start.elapsed();
    println!(
        "Completed transaction scan and dump elapsed time: {:?}",
        elapsed
    );
}

/// Creates a CSV file from the given `SearchReturn` instance.
/// The CSV includes columns for block number, timestamp, transaction details, type, token type, and amount.
/// The timestamp is converted to a human-readable date format.
///
/// # Arguments
///
/// * `input` - A `SearchReturn` instance containing the messages and block timestamps to be written to the CSV.
///
/// # Panics
///
/// * This function will panic if it fails to write to the CSV file.
fn make_csv(input: SearchReturn) {
    let mut wtr = Writer::from_writer(vec![]);
    wtr.write_record(&[
        "Block",
        "Timestamp",
        "Transaction To",
        "Transaction From",
        "Type",
        "Token Type",
        "Amount",
    ])
    .unwrap();

    let mut blocks: Vec<_> = input.messages.keys().cloned().collect();
    blocks.sort();

    for block in blocks {
        let block_timestamp = input.block_timestamps.get(&block).unwrap();
        let datetime =
            chrono::DateTime::<chrono::Utc>::from_timestamp(block_timestamp.seconds, 0).unwrap();
        let formatted_timestamp = datetime.format("%Y-%m-%d %H:%M:%S").to_string();

        if let Some(messages) = input.messages.get(&block) {
            for message in messages {
                match message {
                    MessageWrapper::Send(msg) => {
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.to_address.clone(),
                            msg.from_address.clone(),
                            "SendTokens".to_string(),
                            msg.amount[0].denom.clone(),
                            msg.amount[0].amount.clone(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::Reward {
                        validator,
                        delegator,
                        amounts,
                    } => {
                        for amount in amounts {
                            wtr.write_record(&[
                                block.to_string(),
                                formatted_timestamp.clone(),
                                validator.to_string(),
                                delegator.to_string(),
                                "WithdrawStakingReward".to_string(),
                                amount.denom.clone(),
                                amount.amount.to_string().clone(),
                            ])
                            .unwrap();
                        }
                    }
                    MessageWrapper::Delegate(msg) => {
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.delegator_address.clone(),
                            msg.validator_address.clone(),
                            "Delegate".to_string(),
                            msg.amount.as_ref().unwrap().denom.clone(),
                            msg.amount.as_ref().unwrap().amount.clone(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::UnDelegate(msg) => {
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.validator_address.clone(),
                            msg.delegator_address.clone(),
                            "UnDelegate".to_string(),
                            msg.amount.as_ref().unwrap().denom.clone(),
                            msg.amount.as_ref().unwrap().amount.clone(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::Transfer(msg) => {
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.receiver.clone(),
                            msg.sender.clone(),
                            "IbcTransfer".to_string(),
                            msg.token.as_ref().unwrap().denom.clone(),
                            msg.token.as_ref().unwrap().amount.clone(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::RecvPacket {
                        sender,
                        reciver,
                        amount,
                    } => {
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            reciver.to_string(),
                            sender.to_string(),
                            "IbcRecieve".to_string(),
                            amount.denom.clone(),
                            amount.amount.to_string(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::SendToCosmosClaim(msg) => {
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.cosmos_receiver.clone(),
                            msg.ethereum_sender.clone(),
                            "SendToGravity".to_string(),
                            msg.token_contract.clone(),
                            msg.amount.clone(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::SendToEth(msg) => {
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.eth_dest.clone(),
                            msg.sender.clone(),
                            "SendToEth".to_string(),
                            msg.amount.clone().unwrap().denom.clone(),
                            msg.amount.clone().unwrap().amount.clone(),
                        ])
                        .unwrap();
                    }
                }
            }
        }
    }

    wtr.flush().unwrap();
    let data = String::from_utf8(wtr.into_inner().unwrap()).unwrap();
    std::fs::write("output.csv", data).expect("Failed to write to file");
}
