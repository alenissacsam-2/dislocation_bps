//! Compare the bot's quote for one Raydium CLMM swap against what the program pays.
//!
//! ```text
//! cb-check-quote --as PUBKEY --pool ADDRESS [--rpc URL] [--amount N] [--via POOL]
//! ```
//!
//! Spends token A (the pool's mint 0) into the pool and reads the token B balance the
//! simulation leaves behind, next to the output [`cb_core::path::Leg::quote`] predicted
//! from the same account data. With `--via`, the tool first buys token B on that other
//! pool and then measures the B-to-A direction on `--pool`, so both directions can be
//! checked from a wallet that holds only SOL.
//!
//! Written for a pair the bot kept sending and never landed: SOL/VIDAx across two
//! Raydium CLMM pools. Every one of 433 simulations failed with `TooLittleOutputReceived`
//! on the same pool, whichever direction the cycle ran. A quote the program disagrees
//! with in both directions is a pricing defect, not a race.
//!
//! Like `cb-verify-encode`, it takes a public address only and simulates with
//! `sigVerify` off: no key is read, and nothing can be sent.

use anyhow::{bail, Context, Result};
use cb_executor::encode::{pk, programs, to_pubkey};
use cb_executor::pda::associated_token_address;
use cb_executor::rpc::Rpc;
use cb_executor::venue::raydium::BitmapPolicy;
use cb_executor::venue::{SwapContext, VenueExtra};
use cb_executor::{ticks, tx, venue};
use cb_core::types::Dex;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

struct Pool {
    key: Pubkey,
    data: Vec<u8>,
    mints: [Pubkey; 2],
    programs: [Pubkey; 2],
    fee_ppm: u32,
    tick: i32,
    spacing: u16,
}

async fn load(rpc: &Rpc, key: Pubkey) -> Result<Pool> {
    let data = rpc.accounts(&[key]).await?.pop().flatten().context("the pool does not exist")?;
    let p = cb_dex::raydium_clmm::decode(&data)?;
    let config = rpc
        .accounts(&[to_pubkey(&p.amm_config)])
        .await?
        .pop()
        .flatten()
        .context("the pool's config does not exist")?;
    let fee_ppm = cb_dex::raydium_clmm::decode_trade_fee_ppm(&config)?;
    let mints = [to_pubkey(&p.mint_0), to_pubkey(&p.mint_1)];
    let owners = rpc.accounts_full(&mints).await?;
    let mut progs = [Pubkey::default(); 2];
    for (i, o) in owners.iter().enumerate() {
        progs[i] = o.as_ref().context("a mint does not exist")?.owner;
    }
    Ok(Pool { key, data, mints, programs: progs, fee_ppm, tick: p.tick_current, spacing: p.tick_spacing })
}

fn quote(pool: &Pool, input_is_a: bool, amount: u64) -> Result<u128> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let state = cb_dex::raydium_clmm::to_pool_state(pool.key.to_bytes(), &pool.data, pool.fee_ppm, 0, now)?;
    let input = pool.mints[usize::from(!input_is_a)].to_bytes();
    let leg = state.leg_for_input(&input).context("the pool does not trade that mint")?;
    leg.quote(u128::from(amount)).context("the amount leaves the current tick")
}

async fn swap(rpc: &Rpc, owner: Pubkey, pool: &Pool, input_is_a: bool, amount: u64) -> Result<Instruction> {
    let program = pk(cb_dex::raydium_clmm::PROGRAM_ID);
    let chosen = ticks::resolve(rpc, Dex::RaydiumClmm, &pool.key, &program, pool.tick, pool.spacing, input_is_a).await?;
    let (i, o) = if input_is_a { (0, 1) } else { (1, 0) };
    let ctx = SwapContext {
        owner,
        pool: pool.key,
        user_source: associated_token_address(&owner, &pool.mints[i], &pool.programs[i]),
        user_dest: associated_token_address(&owner, &pool.mints[o], &pool.programs[o]),
        amount_in: amount,
        min_amount_out: 1,
        input_is_a,
        input_token_program: pool.programs[i],
        output_token_program: pool.programs[o],
        tick_arrays: chosen.arrays,
    };
    let extra = VenueExtra { token_program: pool.programs[i], bitmap_policy: BitmapPolicy::Auto };
    venue::build_swap(Dex::RaydiumClmm, &ctx, &pool.data, &extra)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let rpc_url = flag("--rpc")
        .or_else(|| std::env::var("CRYPTOBOT_RPC_HTTP_URL").ok())
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());
    let owner = Pubkey::from_str(&flag("--as").context("--as <public address> is required")?)?;
    let target = Pubkey::from_str(&flag("--pool").context("--pool <address> is required")?)?;
    let amount: u64 = flag("--amount").and_then(|s| s.parse().ok()).unwrap_or(50_000_000);
    let rpc = Rpc::new(&rpc_url)?;

    let pool = load(&rpc, target).await?;
    let wsol = pk(programs::WSOL_MINT);
    if pool.mints[0] != wsol {
        bail!("token A of this pool is not wrapped SOL; this tool funds only from SOL");
    }

    let mut ixs = vec![tx::set_compute_limit(1_000_000)];
    for p in [&pool] {
        for (m, prog) in p.mints.iter().zip(p.programs) {
            let ata = associated_token_address(&owner, m, &prog);
            ixs.push(tx::create_ata_idempotent(&owner, &ata, &owner, m, &prog));
        }
    }
    let wsol_ata = associated_token_address(&owner, &wsol, &pool.programs[0]);
    let b_ata = associated_token_address(&owner, &pool.mints[1], &pool.programs[1]);
    ixs.push(tx::transfer_lamports(&owner, &wsol_ata, amount));
    ixs.push(tx::sync_native(&wsol_ata));

    // What each simulation must be compared with: the balance the watched account had
    // before the measured swap, which a first leg (with --via) changes.
    let (measured_in, input_is_a, watch, predicted) = match flag("--via") {
        None => (amount, true, b_ata, quote(&pool, true, amount)?),
        Some(via) => {
            let first = load(&rpc, Pubkey::from_str(&via)?).await?;
            if first.mints != pool.mints {
                bail!("--via must trade the same two mints");
            }
            ixs.push(swap(&rpc, owner, &first, true, amount).await?);
            // Measure B-to-A with exactly what the first leg is quoted to deliver, less
            // a little, so the input is funded whatever the first leg really pays.
            let got = quote(&first, true, amount)?;
            let spend = u64::try_from(got * 999 / 1000)?;
            (spend, false, wsol_ata, quote(&pool, false, spend)?)
        }
    };

    // Pre-balance of the watched account, from a simulation that stops before the
    // measured swap.
    let (blockhash, _) = rpc.latest_blockhash().await?;
    let pre_tx = tx::compile_unsigned(&owner, &ixs, blockhash)?;
    let pre = rpc.simulate(&pre_tx.tx_base64, &[watch]).await?;
    if let Some(e) = pre.err {
        bail!("the setup alone failed: {e}\n{}", pre.logs.join("\n"));
    }
    let before = pre.post_token_amounts.first().copied().flatten().unwrap_or(0);

    ixs.push(swap(&rpc, owner, &pool, input_is_a, measured_in).await?);
    let full_tx = tx::compile_unsigned(&owner, &ixs, blockhash)?;
    let sim = rpc.simulate(&full_tx.tx_base64, &[watch]).await?;
    if let Some(e) = sim.err {
        bail!("the measured swap failed: {e}\n{}", sim.logs.join("\n"));
    }
    let after = sim.post_token_amounts.first().copied().flatten().context("no balance")?;
    let actual = u128::from(after.saturating_sub(before));

    let diff_bps = (predicted as f64 / actual as f64 - 1.0) * 10_000.0;
    println!("pool      {target} (fee {} ppm, tick {}, spacing {})", pool.fee_ppm, pool.tick, pool.spacing);
    println!("direction {}", if input_is_a { "A to B" } else { "B to A" });
    println!("in        {measured_in}");
    println!("quoted    {predicted}");
    println!("paid      {actual}");
    println!("quote is  {diff_bps:+.2} bps against the program");
    Ok(())
}
