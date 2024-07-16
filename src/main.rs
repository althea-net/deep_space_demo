use clap::Parser;
use cosmos_sdk_proto_althea::cosmos::tx::v1beta1::{TxBody, TxRaw};
use csv::Writer;
use deep_space::utils::historical_grpc_query;
use deep_space::{address::Address, Coin};
use deep_space::{
    client::{types::LatestBlock, Contact},
    utils::decode_any,
};
use futures::future::join_all;
use gravity_proto::auction::{query_client::QueryClient as AuctionQueryClient, Auction};
use gravity_proto::auction::{MsgBid, QueryAuctionByIdRequest};
use log::info;
use prost_types::{Any, Timestamp};
use std::{
    cmp::max,
    collections::HashMap,
    env,
    time::{Duration, Instant},
    vec,
};
use tonic::transport::Channel;

const MSG_BID: &str = "/auction.v1.MsgBid";

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

pub struct MsgBidWrapper {
    pub send: MsgBid,
    pub auction: Auction,
    pub txhash: String,
}

pub struct SearchReturn {
    messages: HashMap<u64, Vec<MsgBidWrapper>>,
    block_timestamps: HashMap<u64, Timestamp>,
}

/// Searches a segment of blocks for auction module transactions
/// returns a Hashmap of transactions indexed by block height
async fn search(
    contact: &Contact,
    mut query_client: AuctionQueryClient<Channel>,
    start: u64,
    end: u64,
) -> SearchReturn {
    let mut blocks = contact.get_block_range(start, end).await;

    while let Err(e) = blocks {
        info!(
            "Failed to get block range {} to {} with error {}, retrying",
            start, end, e
        );
        blocks = contact.get_block_range(start, end).await;
    }
    let blocks = blocks.unwrap();

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
                    MSG_BID => {
                        let send = decode_msg_bid(message);
                        let txs = txs.entry(block_num).or_insert_with(Vec::new);
                        let auction =
                            get_historic_auction(&mut query_client, send.auction_id, block_num)
                                .await;

                        txs.push(MsgBidWrapper {
                            send,
                            auction,
                            txhash: tx_hash.clone(),
                        });
                    }
                    // some other message we don't care about
                    _ => {}
                }
            }
        }
    }
    // info!(
    //     "Got batch of {} blocks, {} contain target messages \n",
    //     blocks_len,
    //     txs.len()
    // );
    SearchReturn {
        messages: txs,
        block_timestamps,
    }
}

/// Gets historical auction data, will search around the time of the transaction for a state snapshot
/// that has the data we want, looking both forward and backward in chain state
async fn get_historic_auction(
    query_client: &mut AuctionQueryClient<Channel>,
    auction_id: u64,
    block_num: u64,
) -> Auction {
    let mut auction = None;
    let mut rounded_block_num = (block_num / 100) * 100;
    let original_founded_block_num = rounded_block_num;
    const TRIES: u64 = 10;
    let mut tires_forward = 0;
    let mut tries_backward = 0;
    while auction.is_none() {
        info!(
            "Querying auction {} at height {}",
            auction_id, rounded_block_num
        );
        let request = QueryAuctionByIdRequest { auction_id };
        let request = historical_grpc_query(request, rounded_block_num);
        auction = query_client
            .auction_by_id(request)
            .await
            .unwrap()
            .into_inner()
            .auction;
        if tries_backward < TRIES {
            rounded_block_num = original_founded_block_num - 100 * tries_backward;
            tries_backward += 1;
        } else if tires_forward < TRIES {
            rounded_block_num = original_founded_block_num + 100 * tires_forward;
            tires_forward += 1;
        } else {
            panic!("Failed to find auction {} in historical data", auction_id);
        }
    }
    auction.unwrap()
}

fn decode_msg_bid(message: Any) -> MsgBid {
    let send_any = prost_types::Any {
        type_url: MSG_BID.to_string(),
        value: message.value,
    };
    decode_any(send_any).unwrap()
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

fn merge_search_results(search_results: Vec<SearchReturn>) -> SearchReturn {
    let mut merged = HashMap::new();
    let mut block_timestamps = HashMap::new();
    for search_result in search_results {
        merged.extend(search_result.messages);
        block_timestamps.extend(search_result.block_timestamps);
    }
    SearchReturn {
        messages: merged,
        block_timestamps,
    }
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
    earliest_block: u64,
    latest_block: u64,
    batch_size: u64,
    execute_size: usize,
) -> SearchReturn {
    let query_client = AuctionQueryClient::connect(contact.get_url())
        .await
        .unwrap();

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
        let fut = search(contact, query_client.clone(), start, end);
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
            info!(
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

    /// Start at a specific block, useful for resuming a scan
    /// Will error if the node does not have the specificed block
    #[arg(short, long)]
    start_at_block: Option<u64>,

    /// How long to wait for a response from a full node before timing out
    /// in seconds. Set this conservatively to avoid crashing an operation
    /// that has already been running for a long time.
    #[arg(short, long, default_value = "30")]
    timeout: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // Initialize the logger
    if env::var("RUST_LOG").is_err() {
        // Set the default logging level to info if RUST_LOG is not set
        env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    let args = Opts::parse();
    let timeout = Duration::from_secs(args.timeout);

    let contact = Contact::new(&args.rpc, timeout, "gravity").expect("invalid url");

    let mut status = contact.get_latest_block().await;
    while status.is_err() {
        info!("Failed to get latest block, retrying");
        status = contact.get_latest_block().await;
    }
    let status = status.unwrap();

    // get the latest block this node has
    let latest_block = match status {
        LatestBlock::Latest { block } | LatestBlock::Syncing { block } => {
            block.header.unwrap().height as u64
        }
        _ => panic!("Node is not synced or not running"),
    };

    // now we find the earliest block this node has via binary search
    let earliest_block = match args.start_at_block {
        Some(block) => block,
        None => get_earliest_block(&contact, 0, latest_block).await,
    };
    // no reason to search blocks before the auction module was deployed
    let earliest_block = max(earliest_block, 9244100);
    info!(
        "This node has {} blocks to download, starting clock now",
        latest_block - earliest_block
    );
    let start = Instant::now();

    let batch_size = args.batch_size;
    let execute_size = args.execute_size;

    let final_merged = download_and_process_blocks(
        &contact,
        earliest_block,
        latest_block,
        batch_size,
        execute_size,
    )
    .await;

    // now we make a csv of the resulting messages
    make_csv(final_merged);

    let elapsed = start.elapsed();
    info!(
        "Completed transaction scan and dump elapsed time: {:?}",
        elapsed
    );
}

const TOKEN_MAPPINGS: &[(&str, &str, u32)] = &[
    ("acanto", "canto", 18),
    ("ugraviton", "graviton", 6),
    (
        "gravity0x07baC35846e5eD502aA91AdF6A9e7aA210F2DcbE",
        "erowan",
        18,
    ),
    (
        "gravity0x35a532d376FFd9a705d0Bb319532837337A398E7",
        "WDOGE",
        18,
    ),
    (
        "gravity0x467719aD09025FcC6cF6F8311755809d45a5E5f3",
        "AXL",
        6,
    ),
    (
        "gravity0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0",
        "WSTETH",
        18,
    ),
    (
        "gravity0x95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE",
        "SHIB",
        18,
    ),
    (
        "gravity0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        "USDC",
        6,
    ),
    (
        "gravity0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        "WETH",
        18,
    ),
    (
        "gravity0xa670d7237398238DE01267472C6f13e5B8010FD1",
        "SOMM",
        6,
    ),
    (
        "gravity0xdAC17F958D2ee523a2206206994597C13D831ec7",
        "USDT",
        6,
    ),
    (
        "gravity0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599",
        "WBTC",
        8,
    ),
    (
        "gravity0x45804880De22913dAFE09f4980848ECE6EcbAf78",
        "PAXG",
        18,
    ),
    (
        "gravity0x514910771AF9Ca656af840dff83E8264EcF986CA",
        "LINK",
        18,
    ),
    (
        "gravity0x6B175474E89094C44Da98b954EedeAC495271d0F",
        "DAI",
        18,
    ),
    (
        "gravity0xfB5c6815cA3AC72Ce9F5006869AE67f18bF77006",
        "PSTAKE",
        18,
    ),
    (
        "gravity0x60e683C6514Edd5F758A55b6f393BeBBAfaA8d5e",
        "PAGE",
        8,
    ),
    (
        "gravity0x77E06c9eCCf2E797fd462A92B6D7642EF85b0A44",
        "WTAO",
        9,
    ),
    (
        "gravity0x92D6C1e31e14520e676a687F0a93788B716BEff5",
        "DYDX",
        18,
    ),
    (
        "gravity0xA0b73E1Ff0B80914AB6fe0444E65848C4C34450b",
        "CRO",
        8,
    ),
    (
        "gravity0xa47c8bf37f92aBed4A126BDA807A7b7498661acD",
        "USTC",
        18,
    ),
    (
        "gravity0xc0a4Df35568F116C370E6a6A6022Ceb908eedDaC",
        "UMEE",
        6,
    ),
    (
        "gravity0xaea46A60368A7bD060eec7DF8CBa43b7EF41Ad85",
        "FET",
        18,
    ),
    (
        "gravity0xe28b3B32B6c345A34Ff64674606124Dd5Aceca30",
        "INJ",
        18,
    ),
    (
        "gravity0x817bbDbC3e8A1204f3691d14bB44992841e3dB35",
        "CUDOS",
        18,
    ),
    (
        "gravity0x817bbDbC3e8A1204f3691d14bB44992841e3dB35",
        "CUDOS",
        18,
    ),
    (
        "gravity0x8FAc8031e079F409135766C7d5De29cf22EF897C",
        "HEART",
        18,
    ),
    (
        "gravity0xAa6E8127831c9DE45ae56bB1b0d4D4Da6e5665BD",
        "ETH2x-FLI",
        18,
    ),
    (
        "gravity0x30f271C9E86D2B7d00a6376Cd96A1cFBD5F0b9b3",
        "DEC",
        18,
    ),
    (
        "gravity0x7D1AfA7B718fb893dB30A3aBc0Cfc608AaCfeBB0",
        "MATIC",
        18,
    ),
    (
        "gravity0xF411903cbC70a74d22900a5DE66A2dda66507255",
        "VERA",
        18,
    ),
    (
        "ibc/AD355DD10DF3C25CD42B5812F34077A1235DF343ED49A633B4E76AE98F3B78BC",
        "USK",
        6,
    ),
    (
        "ibc/3DA3455A6E8EBE1C7EF5C83FDED825B94C13A9303A7FA54C098F13A091B00CE1",
        "UAQLA",
        6,
    ),
    (
        "ibc/97275C664907DF6ADEA732934510F64D0B4EB89886E4DC912AA27A24025E78CD",
        "NEUTARO",
        6,
    ),
    (
        "ibc/4F393C3FCA4190C0A6756CE7F6D897D5D1BE57D6CCB80D0BC87393566A7B6602",
        "STARS",
        6,
    ),
    (
        "ibc/6BEE6DBC35E5CCB3C8ADA943CF446735E6A3D48B174FEE027FAB3410EDE6319C",
        "KUJI",
        6,
    ),
    (
        "ibc/2E5D0AC026AC1AFA65A23023BA4F24BB8DDF94F118EDC0BAD6F625BFC557CDED",
        "ATOM",
        6,
    ),
    (
        "ibc/0C273962C274B2C05B22D9474BFE5B84D6A6FCAD198CB9B0ACD35EA521A36606",
        "NYM",
        6,
    ),
    (
        "ibc/5012B1C96F286E8A6604A87037CE51241C6F1CA195B71D1E261FCACB69FB6BC2",
        "CHEQ",
        9,
    ),
    (
        "ibc/D157AD8A50DAB0FC4EB95BBE1D9407A590FA2CDEE04C90A76C005089BF76E519",
        "FUND",
        9,
    ),
    (
        "ibc/E05A4DAEA5681A09067DC213F32464639D18007215C87964EC45FF876B5EE82B",
        "ARCH",
        18,
    ),
    (
        "ibc/0EB6D5E44D1587D12E222C1155181884098202F56263795259C53536D07C2E65",
        "MEME",
        6,
    ),
    (
        "ibc/00F2B62EB069321A454B708876476AFCD9C23C8C9C4A5A206DDF1CD96B645057",
        "MNTL",
        6,
    ),
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
    wtr.write_record([
        "Timestamp",
        "Bidder",
        "Token Type",
        "Amount",
        "Bid",
        "Winner?",
        "Block",
        "TxHash",
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
                let auction_amount = translate_coin(message.auction.clone().amount.unwrap());
                let bid_amount_coin = Coin {
                    amount: message.send.amount.into(),
                    denom: "ugraviton".to_string(),
                };
                wtr.write_record(&[
                    formatted_timestamp.clone(),
                    message.send.bidder.clone(),
                    auction_amount.denom.clone(),
                    auction_amount.amount.to_string(),
                    translate_coin(bid_amount_coin).amount.to_string(),
                    is_winning_bid(&input, message.auction.id, message.send.amount).to_string(),
                    block.to_string(),
                    message.txhash.clone(),
                ])
                .unwrap();
            }
        }
    }

    wtr.flush().unwrap();
    let data = String::from_utf8(wtr.into_inner().unwrap()).unwrap();
    std::fs::write("auction-data.csv", data).expect("Failed to write to file");
}

fn is_winning_bid(input: &SearchReturn, auction_id: u64, amount: u64) -> bool {
    let mut highest_bid_for_this_auction = 0;
    for messages in input.messages.values() {
        for message in messages {
            if message.auction.id == auction_id {
                highest_bid_for_this_auction =
                    max(highest_bid_for_this_auction, message.send.amount);
            }
        }
    }
    amount == highest_bid_for_this_auction
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CosmosOrEthAddress {
    Cosmos(Address),
    Eth(clarity::Address),
}

impl PartialEq<clarity::Address> for CosmosOrEthAddress {
    fn eq(&self, other: &clarity::Address) -> bool {
        match self {
            CosmosOrEthAddress::Cosmos(_) => false,
            CosmosOrEthAddress::Eth(address) => address == other,
        }
    }
}

impl PartialEq<Address> for CosmosOrEthAddress {
    fn eq(&self, other: &Address) -> bool {
        match self {
            CosmosOrEthAddress::Cosmos(address) => address == other,
            CosmosOrEthAddress::Eth(_) => false,
        }
    }
}

impl ToString for CosmosOrEthAddress {
    fn to_string(&self) -> String {
        match self {
            CosmosOrEthAddress::Cosmos(address) => address.to_string(),
            CosmosOrEthAddress::Eth(address) => address.to_string(),
        }
    }
}

impl std::str::FromStr for CosmosOrEthAddress {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let cosmos = s.parse::<Address>();
        let eth = s.parse::<clarity::Address>();
        match (cosmos, eth) {
            (Ok(cosmos), _) => Ok(CosmosOrEthAddress::Cosmos(cosmos)),
            (_, Ok(eth)) => Ok(CosmosOrEthAddress::Eth(eth)),
            _ => Err("Failed to parse address".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_address_parse() {
        let _address: CosmosOrEthAddress = "0x7d26486cce9ae2ba0eae4f1be92ac379690c723b"
            .parse()
            .unwrap();
    }
}
