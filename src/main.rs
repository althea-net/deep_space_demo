use clap::Parser;
use cosmos_sdk_proto_althea::{
    cosmos::{
        bank::v1beta1::MsgSend,
        distribution::v1beta1::MsgWithdrawDelegatorReward,
        staking::v1beta1::{MsgDelegate, MsgUndelegate},
        tx::v1beta1::{TxBody, TxRaw},
    },
    ibc::applications::transfer::v1::MsgTransfer,
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

    let _blocks_len = blocks.len() as u64;
    for block in blocks {
        let block = block.unwrap();
        let block_num = block.header.clone().unwrap().height as u64;
        let block_timestamp = block.header.unwrap().time.unwrap();
        block_timestamps.insert(block_num, block_timestamp);
        for tx in block.data.unwrap().txs {
            let raw_tx_any = prost_types::Any {
                type_url: "/cosmos.tx.v1beta1.TxRaw".to_string(),
                value: tx.clone(),
            };
            let tx_raw: TxRaw = decode_any(raw_tx_any).unwrap();
            let tx_hash = sha256::digest(tx);
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
                                    let amount = Coin {
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
    // println!(
    //     "Got batch of {} blocks, {} contain target messages \n",
    //     blocks_len,
    //     txs.len()
    // );
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

fn format_duration_long(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    format!("{}d {}h {}m {}s", days, hours, minutes, seconds)
}

fn format_duration_short(duration: Duration) -> String {
    let total_seconds = duration.as_millis();
    let seconds = total_seconds / 1000;
    let ms = total_seconds % 1000;

    format!("{}s {}ms", seconds, ms)
}

/// Downloads blocks and processes transactions in batches.
///
/// # Arguments
///
/// * `contact` - A reference to the Contact struct for interacting with the blockchain.
/// * `target_address` - The address to search for in transactions.
/// * `earliest_block` - The earliest block to start downloading from.
/// * `latest_block` - The latest block to download to.
/// * `batch_size` - The number of blocks each batch should contain.
/// * `execute_size` - The number of batches to execute in parallel.
///
/// # Returns
///
/// * A `SearchReturn` instance containing the combined messages and block timestamps from all downloaded batches.
async fn download_and_process_blocks(
    contact: &Contact,
    target_address: Address,
    earliest_block: u64,
    latest_block: u64,
    batch_size: u64,
    execute_size: usize,
) -> SearchReturn {
    let mut pos = earliest_block;
    let mut futures = Vec::new();
    while pos < latest_block {
        let start = pos;
        let end = if latest_block - pos > batch_size {
            pos += batch_size;
            pos
        } else {
            pos = latest_block;
            latest_block
        };
        let fut = search(&contact, target_address.clone(), start, end);
        futures.push(fut);
    }

    let mut futures = futures.into_iter();

    let mut merged = SearchReturn {
        messages: HashMap::new(),
        block_timestamps: HashMap::new(),
    };
    let mut buf = Vec::new();
    let mut start = Instant::now();
    // this is used to compute the average time per batch in an amortized way
    // we sum the total time constantly into this duration, then divide by how
    // many batches we have processed so far to get the average time per batch
    let mut total_execution_time_so_far = Duration::new(0, 0);
    let mut total_batches_completed_so_far = 0;
    while let Some(fut) = futures.next() {
        if buf.len() < execute_size {
            buf.push(fut);
        } else {
            // run many futures in parallel to download blocks and parse
            let res = join_all(buf).await;
            // merge each result into the total collection of transactions
            // first merge this batch, then merge that into the total
            let batch_merged = merge_search_results(res);
            merged = merge_search_results(vec![merged, batch_merged]);
            // how many blocks we processed this round
            let total_processed = batch_size * execute_size as u64;
            let batches_remaining = futures.len() / execute_size;
            // estimate how long this will take to finish using the average time per batch
            total_execution_time_so_far += start.elapsed();
            total_batches_completed_so_far += 1;
            let estimated_remaining = (total_execution_time_so_far
                / total_batches_completed_so_far)
                * batches_remaining as u32;
            // log information
            println!(
                "Completed batch of {} blocks in {} ETA {}",
                total_processed,
                format_duration_short(start.elapsed()),
                format_duration_long(estimated_remaining)
            );

            start = Instant::now();
            buf = Vec::new();
        }
    }
    let res = join_all(buf).await;
    merge_search_results(vec![merged, merge_search_results(res)])
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

    /// How many blocks a single thread requests
    /// and processes before processing. Higer values increase
    /// memory usage.
    #[arg(short, long, default_value = "1000")]
    batch_size: u64,

    /// How many threads request batches of batch_size blocks
    /// in parallel. Higher values increase memory usage and the
    /// number of requests made to the node.
    #[arg(short, long, default_value = "250")]
    execute_size: usize,
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

    // now we find the earliest block this node has via binary search
    let earliest_block = get_earliest_block(&contact, 0, latest_block).await;
    println!(
        "This node has {} blocks to download, starting clock now",
        latest_block - earliest_block
    );
    let start = Instant::now();

    let batch_size = args.batch_size;
    let execute_size = args.execute_size;

    let final_merged = download_and_process_blocks(
        &contact,
        args.target_account,
        earliest_block,
        latest_block,
        batch_size,
        execute_size,
    )
    .await;

    // now we make a csv of the resulting messages
    make_csv(final_merged);

    let elapsed = start.elapsed();
    println!(
        "Completed transaction scan and dump elapsed time: {:?}",
        elapsed
    );
}

const TOKEN_MAPPINGS: &[(&str, &str, u32)] = &[
    ("acanto", "canto", 18),
    // Add more token mappings here
];

/// Requried to deal with tokens like weth or wbtc
/// where small fractions have a high value
struct FractionalCoin {
    denom: String,
    amount: f64,
}

fn translate_coin<T: Into<Coin>>(coin: T) -> FractionalCoin {
    let coin = coin.into();
    for &(denom, display_denom, decimals) in TOKEN_MAPPINGS {
        if coin.denom == denom {
            let factor = 10u128.pow(decimals);
            let amount = coin.amount.to_string().parse::<f64>().unwrap() / factor as f64;
            return FractionalCoin {
                denom: display_denom.to_string(),
                amount,
            };
        }
    }
    FractionalCoin {
        denom: coin.denom,
        amount: coin.amount.to_string().parse::<f64>().unwrap(),
    }
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
                        let translated_coin = translate_coin(msg.amount[0].clone());
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.to_address.clone(),
                            msg.from_address.clone(),
                            "SendTokens".to_string(),
                            translated_coin.denom.clone(),
                            translated_coin.amount.to_string(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::Reward {
                        validator,
                        delegator,
                        amounts,
                    } => {
                        for amount in amounts {
                            let translated_coin = translate_coin(amount.clone());
                            wtr.write_record(&[
                                block.to_string(),
                                formatted_timestamp.clone(),
                                validator.to_string(),
                                delegator.to_string(),
                                "WithdrawStakingReward".to_string(),
                                translated_coin.denom.clone(),
                                translated_coin.amount.to_string().clone(),
                            ])
                            .unwrap();
                        }
                    }
                    MessageWrapper::Delegate(msg) => {
                        let translated_coin = translate_coin(msg.clone().amount.unwrap());
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.delegator_address.clone(),
                            msg.validator_address.clone(),
                            "Delegate".to_string(),
                            translated_coin.denom.clone(),
                            translated_coin.amount.to_string(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::UnDelegate(msg) => {
                        let translated_coin = translate_coin(msg.clone().amount.unwrap());
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.validator_address.clone(),
                            msg.delegator_address.clone(),
                            "UnDelegate".to_string(),
                            translated_coin.denom.clone(),
                            translated_coin.amount.to_string(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::Transfer(msg) => {
                        let translated_coin = translate_coin(msg.clone().token.unwrap());
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.receiver.clone(),
                            msg.sender.clone(),
                            "IbcTransfer".to_string(),
                            translated_coin.denom.clone(),
                            translated_coin.amount.to_string(),
                        ])
                        .unwrap();
                    }
                    MessageWrapper::RecvPacket {
                        sender,
                        reciver,
                        amount,
                    } => {
                        let translated_coin = translate_coin(amount.clone());
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            reciver.to_string(),
                            sender.to_string(),
                            "IbcRecieve".to_string(),
                            translated_coin.denom.clone(),
                            translated_coin.amount.to_string(),
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
                        let translated_coin = translate_coin(msg.amount.clone().unwrap());
                        wtr.write_record(&[
                            block.to_string(),
                            formatted_timestamp.clone(),
                            msg.eth_dest.clone(),
                            msg.sender.clone(),
                            "SendToEth".to_string(),
                            translated_coin.denom.clone(),
                            translated_coin.amount.to_string(),
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
