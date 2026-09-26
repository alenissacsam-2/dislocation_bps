//! Check the compressed re-price read against the plain one on live accounts.
//!
//! `cargo run -p cb-executor --example check_zstd_read -- <rpc url>`
//!
//! Reads the first 40 pools in the registry both ways and compares owner, length and
//! bytes. Pool accounts move between two reads a few milliseconds apart, so a
//! handful of byte differences on busy pools is expected; a length or owner
//! difference, or a missing account, is not.

use cb_executor::rpc::Rpc;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args().nth(1).expect("pass an RPC URL");
    let rpc = Rpc::new(url)?;
    let raw = std::fs::read_to_string("crates/bot/pools.json")?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    let keys: Vec<Pubkey> = v["pools"]
        .as_array()
        .expect("pools")
        .iter()
        .take(40)
        .filter_map(|p| Pubkey::from_str(p["address"].as_str()?).ok())
        .collect();
    let t = std::time::Instant::now();
    let zstd = rpc.accounts_latest(&keys).await?;
    let zstd_ms = t.elapsed().as_millis();
    let plain = rpc.accounts_full(&keys).await?;
    let (mut same, mut moved, mut wrong) = (0, 0, 0);
    for (a, b) in zstd.iter().zip(&plain) {
        match (a, b) {
            (Some(a), Some(b)) if a.owner == b.owner && a.data.len() == b.data.len() => {
                if a.data == b.data { same += 1 } else { moved += 1 }
            }
            (None, None) => same += 1,
            _ => wrong += 1,
        }
    }
    println!("{} accounts in {zstd_ms} ms: {same} identical, {moved} moved between reads, {wrong} wrong", keys.len());
    anyhow::ensure!(wrong == 0, "the compressed read disagrees with the plain one");
    Ok(())
}
