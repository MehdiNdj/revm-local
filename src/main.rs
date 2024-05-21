use ethers_core::types::BlockId;
use ethers_providers::Middleware;
use ethers_providers::{Http, Provider};
use indicatif::ProgressBar;
use revm::db::{CacheDB, EthersDB, StateBuilder};
use revm::primitives::{Address, TransactTo, U256};
use revm::{inspector_handle_register, Evm, GetInspector};
use revm::inspectors::TracerEip3155;
use revm_primitives::Output;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;
use std::io::Cursor;
use std::io::{Read, Write};

macro_rules! local_fill {
    ($left:expr, $right:expr, $fun:expr)     => {
        if let Some(right) = $right {
            $left = $fun(right.0)
        }
    };
    ($left:expr, $right:expr) => {
        if let Some(right) = $right {
            $left = Address::from(right.as_fixed_bytes())
        }
    };
}

struct FlushWriter {
    pub writer: Arc<Mutex<std::io::Cursor<Vec<u8>>>>,
}

impl FlushWriter {
    fn new(writer: Arc<Mutex<std::io::Cursor<Vec<u8>>>>) -> Self {
        Self { writer }
    }
}

impl Write for FlushWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.lock().unwrap().flush()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create ethers client and wrap it in Arc<M>
    let client = Provider::<Http>::try_from(
        "https://eth.llamarpc.com",
    )?;
    let client = Arc::new(client);

    // Params
    let chain_id: u64 = 1;
    let block_number = 19810971;

    // Fetch the transaction-rich block
    let block = match client.get_block_with_txs(block_number).await {
        Ok(Some(block)) => block,
        Ok(None) => anyhow::bail!("Block not found"),
        Err(error) => anyhow::bail!("Error: {:?}", error),
    };
    println!("Fetched block number: {}", block.number.unwrap().0[0]);
    let previous_block_number = block_number - 1;

    // Use the previous block state as the db with caching
    let prev_id: BlockId = previous_block_number.into();
    // SAFETY: This cannot fail since this is in the top-level tokio runtime
    let state_db = EthersDB::new(Arc::clone(&client), Some(prev_id)).expect("panic");
    let cache_db: CacheDB<EthersDB<Provider<Http>>> = CacheDB::new(state_db);
    let mut state = StateBuilder::new_with_database(cache_db).build();
    let mut evm = Evm::builder()
        .with_db(&mut state)
        .with_external_context(TracerEip3155::new(Box::new(std::io::stdout())))
        .modify_block_env(|b| {
            if let Some(number) = block.number {
                let nn = number.0[0];
                b.number = U256::from(nn);
            }
            local_fill!(b.coinbase, block.author);
            local_fill!(b.timestamp, Some(block.timestamp), U256::from_limbs);
            local_fill!(b.difficulty, Some(block.difficulty), U256::from_limbs);
            local_fill!(b.gas_limit, Some(block.gas_limit), U256::from_limbs);
            if let Some(base_fee) = block.base_fee_per_gas {
                local_fill!(b.basefee, Some(base_fee), U256::from_limbs);
            }
        })
        .modify_cfg_env(|c| {
            c.chain_id = chain_id;
        })
        .append_handler_register(inspector_handle_register)
        .build();

    let txs = block.transactions.len();
    println!("Found {txs} transactions.");

    let console_bar = Arc::new(ProgressBar::new(txs as u64));
    let start = Instant::now();

    // Create the traces directory if it doesn't exist
    std::fs::create_dir_all("traces").expect("Failed to create traces directory");

    // Fill in CfgEnv
    for tx in block.transactions {
        evm = evm
            .modify()
            .modify_tx_env(|etx| {
                etx.caller = Address::from(tx.from.as_fixed_bytes());
                etx.gas_limit = tx.gas.as_u64();
                local_fill!(etx.gas_price, tx.gas_price, U256::from_limbs);
                local_fill!(etx.value, Some(tx.value), U256::from_limbs);
                etx.data = tx.input.0.into();
                let mut gas_priority_fee = U256::ZERO;
                local_fill!(
                    gas_priority_fee,
                    tx.max_priority_fee_per_gas,
                    U256::from_limbs
                );
                etx.gas_priority_fee = Some(gas_priority_fee);
                etx.chain_id = Some(chain_id);
                etx.nonce = Some(tx.nonce.as_u64());
                if let Some(access_list) = tx.access_list {
                    etx.access_list = access_list
                        .0
                        .into_iter()
                        .map(|item| {
                            let new_keys: Vec<U256> = item
                                .storage_keys
                                .into_iter()
                                .map(|h256| U256::from_le_bytes(h256.0))
                                .collect();
                            (Address::from(item.address.as_fixed_bytes()), new_keys)
                        })
                        .collect();
                } else {
                    etx.access_list = Default::default();
                }

                etx.transact_to = match tx.to {
                    Some(to_address) => {
                        TransactTo::Call(Address::from(to_address.as_fixed_bytes()))
                    }
                    None => TransactTo::create(),
                };
            })
            .build();


        // Construct the file writer to write the trace to
        let tx_number = tx.transaction_index.unwrap().0[0];
        let write = Cursor::new(Vec::new());
        let inner = Arc::new(Mutex::new(
            write,
        ));
        let writer = FlushWriter::new(Arc::clone(&inner));
        // Inspect and commit the transaction to the EVM
        evm.context.external.set_writer(Box::new(writer));       
        if let Err(error) = evm.transact_commit() {
            println!("Got error: {:?}", error);
        }

        dbg!(&tx_number);
        let tmp = inner.lock().unwrap();
        let mut value = String::from_utf8(tmp.get_ref().to_vec()).unwrap();

        // They don't respect JSON standard so we need some patching here
        value = value.replace("\n", ",");
        value = "[".to_string() + &value[..(value.len()-1)] + "]"; // we remove the trailing comma and have everything in a array

        let value: serde_json::Value = serde_json::from_str(&value).unwrap();
        dbg!(&value);

        console_bar.inc(1);
    }

    console_bar.finish_with_message("Finished all transactions.");

    let elapsed = start.elapsed();
    println!(
        "Finished execution. Total CPU time: {:.6}s",
        elapsed.as_secs_f64()
    );

    Ok(())
}