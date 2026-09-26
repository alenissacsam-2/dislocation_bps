//! Check the hand-encoded lookup-table instructions against mainnet without signing or
//! sending anything: simulate "create a table, then extend it" for a funded address
//! with signature verification off.
//!
//! `cargo run -p cb-executor --example sim_lookup_table -- <funded address> [rpc url]`
use cb_executor::{alt, rpc::Rpc, tx};
use solana_sdk::pubkey::Pubkey;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let payer: Pubkey = std::env::args().nth(1).expect("a funded address").parse()?;
    let url = std::env::args().nth(2).unwrap_or_else(|| "https://api.mainnet-beta.solana.com".into());
    let rpc = Rpc::new(url)?;
    let slot = rpc.slot().await?;
    let (create, table) = alt::create(&payer, &payer, slot);
    let keys: Vec<Pubkey> = (0..5).map(|_| Pubkey::new_unique()).collect();
    let extend = alt::extend(&table, &payer, &payer, &keys)?;
    let (blockhash, _) = rpc.latest_blockhash().await?;
    let unsigned = tx::compile_unsigned(&payer, &[create, extend], blockhash)?;
    let sim = rpc.simulate(&unsigned.tx_base64, &[]).await?;
    println!("table {table} at slot {slot}: succeeded={} err={:?}", sim.succeeded(), sim.err);
    for l in sim.logs.iter().take(12) {
        println!("  {l}");
    }
    Ok(())
}
