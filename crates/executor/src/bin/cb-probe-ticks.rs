//! A throwaway diagnostic: find which tick-array addresses a pool actually has.
//!
//! Used to settle, by measurement rather than by reading somebody's SDK, what the
//! array-size rule is for a venue whose derived arrays did not exist. Give it a pool
//! address; it sweeps candidate array sizes and start indices and reports which of the
//! derived addresses the chain actually holds an account at.
//!
//! ```text
//! cb-probe-ticks <pool-address> [--rpc URL] [--orca]
//! ```

use anyhow::{bail, Context, Result};
use cb_executor::encode::pk;
use cb_executor::pda::{orca_tick_array, raydium_tick_array};
use cb_executor::rpc::Rpc;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let Some(addr) = args.get(1).filter(|a| !a.starts_with("--")) else {
        bail!("usage: cb-probe-ticks <pool-address> [--rpc URL] [--orca]");
    };
    let orca = args.iter().any(|a| a == "--orca");
    let rpc_url = args
        .iter()
        .position(|a| a == "--rpc")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());

    let pool = Pubkey::from_str(addr).context("not a pubkey")?;
    let rpc = Rpc::new(&rpc_url)?;
    let data = rpc
        .accounts(&[pool])
        .await?
        .into_iter()
        .next()
        .flatten()
        .context("the pool account does not exist")?;

    let (tick, spacing, liquidity, program) = if orca {
        let w = cb_dex::orca_whirlpool::decode(&data)?;
        (w.tick_current, w.tick_spacing, w.liquidity, pk(cb_dex::orca_whirlpool::PROGRAM_ID))
    } else {
        let p = cb_dex::raydium_clmm::decode(&data)?;
        (p.tick_current, p.tick_spacing, p.liquidity, pk(cb_dex::raydium_clmm::PROGRAM_ID))
    };

    println!("pool      {pool}");
    println!("account   {} bytes", data.len());
    println!("tick      {tick}");
    println!("spacing   {spacing}");
    println!("liquidity {liquidity}");
    println!();

    // Sweep plausible array sizes. If any size makes the derived address resolve, that
    // size is the rule; if none does, the seed scheme itself is wrong.
    let wide = args.iter().any(|a| a == "--wide");
    let sizes: &[i32] = if wide { &[60] } else { &[60, 88, 64, 100, 120, 512, 1024] };
    for size in sizes.iter().copied() {
        let span = i32::from(spacing) * size;
        if span == 0 {
            continue;
        }
        let base = tick.div_euclid(span) * span;
        let reach = if wide { 40 } else { 2 };
        let candidates: Vec<i32> = (-reach..=reach).map(|k| base + k * span).collect();
        let keys: Vec<Pubkey> = candidates
            .iter()
            .map(|s| {
                if orca {
                    orca_tick_array(&pool, *s, &program)
                } else {
                    raydium_tick_array(&pool, *s, &program)
                }
            })
            .collect();
        let found = rpc.accounts_full(&keys).await?;
        let hits: Vec<String> = candidates
            .iter()
            .zip(found.iter())
            .filter_map(|(s, f)| f.as_ref().map(|a| format!("{s} ({} bytes, owner {})", a.data.len(), a.owner)))
            .collect();
        println!(
            "size {size:>5} (span {span:>7}) base {base:>9}  ->  {}",
            if hits.is_empty() { "nothing".to_string() } else { hits.join(", ") }
        );
    }

    Ok(())
}
