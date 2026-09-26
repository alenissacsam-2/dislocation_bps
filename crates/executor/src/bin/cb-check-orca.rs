//! Check Orca Whirlpool pricing — adaptive fee included — against the program, on a live
//! pool.
//!
//! ```text
//! cb-check-orca --as PUBKEY --pool ADDRESS [--rpc URL] [--amount LAMPORTS]
//! ```
//!
//! Simulates, from a public address with `sigVerify` off (no key, nothing sent), a swap
//! of SOL into the pool and then of 90% of the output back, and compares each with the
//! bot's own quote. An adaptive-fee pool is priced from its oracle
//! (`cb_dex::orca_whirlpool::to_pool_state_adaptive`) over a window of block times around
//! now; leg two is priced on the pool *and oracle* as leg one left them, read from the
//! simulation's post-state, since leg one moves both.

use anyhow::{bail, Context, Result};
use cb_core::types::Dex;
use cb_dex::orca_whirlpool as orca;
use cb_executor::encode::{pk, programs, to_pubkey};
use cb_executor::pda::{associated_token_address, orca_oracle};
use cb_executor::rpc::Rpc;
use cb_executor::venue::{SwapContext, VenueExtra};
use cb_executor::{ticks, tx, venue};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// The window of block times a quote is priced over, around the wall clock: the chain's
/// clock can lag it by a few seconds, and a trade lands a moment after it is priced.
const BEHIND_SECS: u64 = 10;
const AHEAD_SECS: u64 = 20;

fn quote(address: Pubkey, pool: &[u8], oracle: Option<&[u8]>, input: &Pubkey, x: u64) -> Result<(u128, u32)> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let state = match oracle {
        Some(o) => orca::to_pool_state_adaptive(
            address.to_bytes(),
            pool,
            &orca::decode_oracle(o)?,
            now - BEHIND_SECS,
            now + AHEAD_SECS,
            0,
        )?,
        None => orca::to_pool_state(address.to_bytes(), pool, 0)?,
    };
    let leg = state.leg_for_input(&input.to_bytes()).context("the pool does not trade that mint")?;
    let out = leg.quote(u128::from(x)).context("the amount leaves the current tick group")?;
    Ok((out, state.fee_ppm))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let url = flag("--rpc").or_else(|| std::env::var("CRYPTOBOT_RPC_HTTP_URL").ok()).context("--rpc")?;
    let rpc = Rpc::new(url)?;
    let owner = Pubkey::from_str(&flag("--as").context("--as <public address>")?)?;
    let address = Pubkey::from_str(&flag("--pool").context("--pool <address>")?)?;
    let amount: u64 = flag("--amount").and_then(|s| s.parse().ok()).unwrap_or(5_000_000);
    let program = pk(orca::PROGRAM_ID);
    let oracle_key = orca_oracle(&address, &program);

    let got = rpc.accounts_full(&[address, oracle_key]).await?;
    let data = got[0].as_ref().context("no pool")?.data.clone();
    let oracle = got[1].as_ref().filter(|a| a.owner == program).map(|a| a.data.clone());
    let w = orca::decode(&data)?;
    if let Some(o) = oracle.as_deref() {
        let o = orca::decode_oracle(o)?;
        println!(
            "oracle {oracle_key}: group {} filter {}s decay {}s reduction {} control {} max acc {}; \
             reference {} at group {}, accumulator {}, updated {} (major {})",
            o.tick_group_size,
            o.filter_period,
            o.decay_period,
            o.reduction_factor,
            o.adaptive_fee_control_factor,
            o.max_volatility_accumulator,
            o.volatility_reference,
            o.tick_group_index_reference,
            o.volatility_accumulator,
            o.last_reference_update_timestamp,
            o.last_major_swap_timestamp
        );
    }
    let (m_a, m_b) = (to_pubkey(&w.mint_a), to_pubkey(&w.mint_b));
    let owners = rpc.accounts_full(&[m_a, m_b]).await?;
    let prog_a = owners[0].as_ref().context("mint a")?.owner;
    let prog_b = owners[1].as_ref().context("mint b")?.owner;
    let wsol = pk(programs::WSOL_MINT);
    let sol_is_a = if m_a == wsol { true } else if m_b == wsol { false } else { bail!("neither side is SOL") };
    println!(
        "pool {address}: static fee {} ppm, spacing {}, marked adaptive {}, oracle {}",
        w.fee_rate_ppm,
        w.tick_spacing,
        w.adaptive,
        if oracle.is_some() { "initialised" } else { "absent" }
    );

    let ata_a = associated_token_address(&owner, &m_a, &prog_a);
    let ata_b = associated_token_address(&owner, &m_b, &prog_b);
    let (wsol_ata, token_ata) = if sol_is_a { (ata_a, ata_b) } else { (ata_b, ata_a) };
    let token_mint = if sol_is_a { m_b } else { m_a };
    let mut ixs: Vec<Instruction> = vec![tx::set_compute_limit(1_000_000)];
    ixs.push(tx::create_ata_idempotent(&owner, &ata_a, &owner, &m_a, &prog_a));
    ixs.push(tx::create_ata_idempotent(&owner, &ata_b, &owner, &m_b, &prog_b));
    ixs.push(tx::transfer_lamports(&owner, &wsol_ata, amount));
    ixs.push(tx::sync_native(&wsol_ata));

    let swap = |pool: &[u8], input_is_a: bool, amount_in: u64, arrays: [Pubkey; 3]| {
        let ctx = SwapContext {
            owner,
            pool: address,
            user_source: if input_is_a { ata_a } else { ata_b },
            user_dest: if input_is_a { ata_b } else { ata_a },
            amount_in,
            min_amount_out: 1,
            input_is_a,
            input_token_program: if input_is_a { prog_a } else { prog_b },
            output_token_program: if input_is_a { prog_b } else { prog_a },
            tick_arrays: arrays,
        };
        venue::build_swap(Dex::OrcaWhirlpool, &ctx, pool, &VenueExtra::default())
    };
    let (blockhash, _) = rpc.latest_blockhash().await?;
    let sim = |ixs: Vec<Instruction>, watch: Vec<Pubkey>| {
        let rpc = &rpc;
        async move {
            let t = tx::compile_unsigned(&owner, &ixs, blockhash)?;
            let s = rpc.simulate(&t.tx_base64, &watch).await?;
            if let Some(e) = &s.err {
                bail!("simulation failed: {e}\n{}", s.logs.join("\n"));
            }
            anyhow::Ok((s.post_token_amounts.iter().map(|a| a.unwrap_or(0)).collect::<Vec<u64>>(), s.post_data))
        }
    };

    // Leg one: SOL in, on the pool and oracle as they stand.
    let arrays = ticks::resolve(&rpc, Dex::OrcaWhirlpool, &address, &program, w.tick_current, w.tick_spacing, sol_is_a)
        .await?
        .arrays;
    let (pre, _) = sim(ixs.clone(), vec![token_ata]).await?;
    let mut one = ixs.clone();
    one.push(swap(&data, sol_is_a, amount, arrays)?);
    let (post_one, one_data) = sim(one.clone(), vec![token_ata, wsol_ata, address, oracle_key]).await?;
    let got = post_one[0] - pre[0];
    let (q, fee) = quote(address, &data, oracle.as_deref(), &wsol, amount)?;
    println!("leg 1  spend {amount}: paid {got}, quoted {q} at {fee} ppm ({:+.4} bps; must be <= 0)", (q as f64 / got as f64 - 1.0) * 1e4);

    // Leg two: 90% of it back, on the pool and oracle leg one left.
    let pool_after = one_data[2].clone().context("pool after leg one")?;
    let oracle_after = if oracle.is_some() { one_data[3].clone() } else { None };
    let w_after = orca::decode(&pool_after)?;
    let back = got * 9 / 10;
    let arrays = ticks::resolve(&rpc, Dex::OrcaWhirlpool, &address, &program, w_after.tick_current, w.tick_spacing, !sol_is_a)
        .await?
        .arrays;
    let mut two = one.clone();
    two.push(swap(&pool_after, !sol_is_a, back, arrays)?);
    let (post_two, _) = sim(two, vec![wsol_ata]).await?;
    let got2 = post_two[0] - post_one[1];
    let (q2, fee2) = quote(address, &pool_after, oracle_after.as_deref(), &token_mint, back)?;
    println!("leg 2  spend {back}: paid {got2}, quoted {q2} at {fee2} ppm ({:+.4} bps; must be <= 0)", (q2 as f64 / got2 as f64 - 1.0) * 1e4);
    Ok(())
}
