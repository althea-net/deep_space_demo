use accounts::get_balances_for_accounts;
use cosmos_sdk_proto::{
    cosmos::tx::v1beta1::{TxBody, TxRaw},
    ibc::applications::transfer::v1::MsgTransfer,
};
use deep_space::{client::Contact, utils::decode_any, Coin};
use eth_deposits::check_for_events;
use futures::future::join_all;
use gravity_proto::gravity::MsgSendToEth;
use lazy_static::lazy_static;
use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use web30::client::Web3;

mod accounts;
mod eth_deposits;

lazy_static! {
    static ref COUNTER: Arc<RwLock<Counters>> = Arc::new(RwLock::new(Counters {
        blocks: 0,
        transactions: 0,
        msgs: 0
    }));
}

pub struct Counters {
    blocks: u64,
    transactions: u64,
    msgs: u64,
}

const TIMEOUT: Duration = Duration::from_secs(5);

async fn search(contact: &Contact, start: u64, end: u64) {
    let mut blocks = contact.get_block_range(start, end).await;

    while blocks.is_err() {
        blocks = contact.get_block_range(start, end).await;
        std::thread::sleep(Duration::from_secs(10));
    }
    let blocks = blocks.unwrap();

    let mut tx_counter = 0;
    let mut msg_counter = 0;
    let blocks_len = blocks.len() as u64;
    for block in blocks {
        let block = block.unwrap();
        for tx in block.data.unwrap().txs {
            tx_counter += 1;

            let raw_tx_any = prost_types::Any {
                type_url: "/cosmos.tx.v1beta1.TxRaw".to_string(),
                value: tx,
            };
            let tx_raw: TxRaw = decode_any(raw_tx_any).unwrap();
            let body_any = prost_types::Any {
                type_url: "/cosmos.tx.v1beta1.TxBody".to_string(),
                value: tx_raw.body_bytes,
            };
            let tx_body: TxBody = decode_any(body_any).unwrap();
            msg_counter += tx_body.messages.len() as u64;

            // boilerplate is finished, now we display various message types

            display_ibc_transfer(&tx_body);
            display_msg_send_to_eth(&tx_body);
        }
    }
    let mut c = COUNTER.write().unwrap();
    c.blocks += blocks_len;
    c.transactions += tx_counter;
    c.msgs += msg_counter;
}

fn display_ibc_transfer(tx_body: &TxBody) {
    for message in tx_body.messages.iter() {
        if message.type_url.contains("MsgTransfer") {
            let ibc_transfer_any = prost_types::Any {
                type_url: "/ibc.applications.v1.MsgTransfer".to_string(),
                value: message.value.clone(),
            };
            let ibc_transfer: Result<MsgTransfer, _> = decode_any(ibc_transfer_any);

            if let Ok(decoded_transfer) = ibc_transfer {
                let coin: Coin = decoded_transfer.token.unwrap().into();
                println!(
                    "IBC transfer by {} to {} of {}",
                    decoded_transfer.sender, decoded_transfer.receiver, coin
                );
            }
        }
    }
}

fn display_msg_send_to_eth(tx_body: &TxBody) {
    for message in tx_body.messages.iter() {
        if message.type_url.contains("MsgSendToEth") {
            let send_to_eth_any = prost_types::Any {
                type_url: "/gravity.v1.MsgSendToEth".to_string(),
                value: message.value.clone(),
            };
            let send_to_eth: Result<MsgSendToEth, _> = decode_any(send_to_eth_any);

            if let Ok(decoded_transfer) = send_to_eth {
                let coin: Coin = decoded_transfer.amount.unwrap().into();
                println!(
                    "Msg send to ETH by {} to {} of {}",
                    decoded_transfer.sender, decoded_transfer.eth_dest, coin
                );
            }
        }
    }
}

async fn display_all_accounts(contact: &Contact) {
    let accounts = contact.get_all_accounts().await.unwrap();
    let balances = get_balances_for_accounts(accounts, "ugraviton".to_string())
        .await
        .unwrap();
    for user in balances {
        println!(
            "User {} has balance {}ugraviton",
            user.account.get_base_account().address,
            user.balance
        )
    }
}

async fn get_gravity_info() {
    // adjust these for your desired block range
    let block_end = 6071405;
    let block_start = 4071405;

    let contact = Contact::new(GRAVITY_NODE_GRPC, TIMEOUT, "gravity").expect("invalid url");

    let _status = contact
        .get_chain_status()
        .await
        .expect("Failed to get chain status, grpc error");

    println!("Getting all account balances");
    display_all_accounts(&contact).await;

    let start = Instant::now();

    // build batches of futures for downloading blocks
    const BATCH_SIZE: u64 = 100;
    const EXECUTE_SIZE: usize = 1000;
    let latest_block = block_end;
    let earliest_block = block_start;
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
        let fut = search(&contact, start, end);
        futures.push(fut);
    }

    // remove the rev here to go oldest to newest instead of newest to oldest
    let mut futures = futures.into_iter().rev();

    // execute all those futures in highly parallel batches
    let mut buf = Vec::new();
    while let Some(fut) = futures.next() {
        if buf.len() < EXECUTE_SIZE {
            buf.push(fut);
        } else {
            let _ = join_all(buf).await;
            buf = Vec::new();
        }
    }
    let _ = join_all(buf).await;

    let counter = COUNTER.read().unwrap();
    println!(
        "Successfully downloaded {} blocks and {} tx containing {} messages in {} seconds",
        counter.blocks,
        counter.transactions,
        counter.msgs,
        start.elapsed().as_secs()
    )
}

async fn get_eth_info() {
    // eth info
    let eth_end_block = 16786713u64.into();
    let eth_start_block = 12786713u64.into();
    let web3 = Web3::new(ETH_NODE_RPC, Duration::from_secs(60));
    check_for_events(
        &web3,
        "0xa4108aA1Ec4967F8b52220a4f7e94A8201F2D906"
            .parse()
            .unwrap(),
        eth_start_block,
        eth_end_block,
    )
    .await;
}

pub const GRAVITY_NODE_GRPC: &str = "http://gravitychain.io:9090";
pub const ETH_NODE_RPC: &str = "https://eth.althea.net";

#[actix_rt::main]
async fn main() {
    // eth info, just deposits from ETH
    //get_eth_info().await;
    // gravity info
    // includes transfers back to eth
    // ibc tranfsers
    // and a full account list
    get_gravity_info().await;
}
