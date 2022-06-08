use deep_space::client::Contact;
use futures::future::join_all;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(5);

/// finds earliest available block using binary search, keep in mind this cosmos
/// node will not have history from chain halt upgrades and could be state synced
/// and missing history before the state sync
/// Iterative implementation due to the limitations of async recursion in rust.
async fn get_earliest_block(contact: &Contact, mut start: u64, mut end: u64) -> u64 {
    while start != end {
        let mid = start + (end - start) / 2;
        let mid_block = contact.get_block(mid).await;
        if let Ok(Some(_)) = mid_block {
            end = mid - 1;
        } else {
            start = mid + 1;
        }
    }
    start
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let contact =
        Contact::new("http://gravitychain.io:9090", TIMEOUT, "gravity").expect("invalid url");
    let mut blocks = HashMap::new();

    let status = contact
        .get_chain_status()
        .await
        .expect("Failed to get chain status, grpc error");

    // get the latest block this node has
    let latest_block = match status {
        deep_space::client::ChainStatus::Moving { block_height } => block_height,
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

    // the actual block download logic, we're building an array of futures which we'll
    // hand off to be executed in parallel, note that for gravitychain.io it's a node cluster
    // meaning not all nodes have the exact same number of blocks (they are state synced) so
    // we may download a smaller number of blocks than the latest block we find above as our
    // connection is load balanced across the cluster to nodes with less history.

    // number of blocks to request as a single future, if we did single blocks
    // the overhead of each connection would reduce total speed, a more complete
    // implementation would auto-tune this value and not hand everything off to tokio
    // at once, in order to really optimize performance given latency to the cluster.
    const BATCH_SIZE: u64 = 100;
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
        let fut = contact.get_block_range(start, end);
        futures.push(fut);
    }

    // this is where we actually hand off all the futures to tokio to execute in parallel
    // splitting this up to run lets say 10k at a time would reduce memory usage and allow
    // indexing the results to a local database (sled or rocksdb) for easy review and instant re-runs
    let results = join_all(futures).await;

    // now we take the results, index them ignoring any that failed due to timeouts or
    // other issues, a proper implementation would keep track of failures here and build
    // another run specifically of failures.
    for result in results.into_iter().flatten() {
        for block in result.into_iter().flatten() {
            blocks.insert(block.last_commit.clone().unwrap().height, block);
        }
    }

    println!(
        "Successfully downloaded {} blocks in {} seconds",
        blocks.len(),
        start.elapsed().as_secs()
    )
}
