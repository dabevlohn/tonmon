use tonlib_client::client::TonClient;
use tonlib_client::client::TonClientInterface;
use tonlib_client::tl::BlockId;
use tonlib_client::tl::BlocksMasterchainInfo;
use tonlib_client::tl::BlocksShards;
use tonlib_client::tl::BlocksTransactions;
use tonlib_client::tl::InternalTransactionId;
use tonlib_client::tl::NULL_BLOCKS_ACCOUNT_TRANSACTION_ID;
use tonlib_core::types::ZERO_HASH;
use tonlib_core::TonAddress;
use tonlib_core::TonHash;

async fn call_blockchain_methods() -> anyhow::Result<()> {
    let client = TonClient::builder().build().await?;
    let (_, info) = client.get_masterchain_info().await?;
    // println!("MasterchainInfo: {:?}", &info);
    let block_id = BlockId {
        workchain: info.last.workchain,
        shard: info.last.shard,
        seqno: info.last.seqno,
    };
    let block_id_ext = client.lookup_block(1, &block_id, 0, 0).await?;
    println!("BlockIdExt: {:?}", &block_id_ext);
    let block_shards: BlocksShards = client.get_block_shards(&info.last).await?;
    let mut shards = block_shards.shards.clone();
    // println!("Shards: {:?}", &block_shards);
    shards.insert(0, info.last.clone());
    for shard in &shards {
        println!("Processing shard: {:?}", shard);
        let workchain = shard.workchain;
        let txs: BlocksTransactions = client
            .get_block_transactions(&shard, 7, 1024, &NULL_BLOCKS_ACCOUNT_TRANSACTION_ID)
            .await?;
        println!(
            "Number of transactions: {}, incomplete: {}",
            txs.transactions.len(),
            txs.incomplete
        );
        for tx_id in txs.transactions {
            let t = TonHash::try_from(tx_id.account.as_slice())?;
            let addr = TonAddress::new(workchain, t);
            let id = InternalTransactionId {
                hash: tx_id.hash.clone(),
                lt: tx_id.lt,
            };
            let tx = client.get_raw_transactions_v2(&addr, &id, 1, false).await?;
            println!("Tx: {:?}", tx.transactions[0])
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    call_blockchain_methods().await?;
    Ok(())
}
