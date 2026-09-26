//! Check Raydium CP-Swap encoding and pricing against the program, on a live pool.
//!
//! ```text
//! cb-check-cpmm --as PUBKEY --pool ADDRESS [--rpc URL] [--amount LAMPORTS]
//! ```
//!
//! Simulates, from a public address with `sigVerify` off (no key, nothing sent), a swap
//! of SOL into the pool and then of 90% of the output back, and compares each with the
//! bot's own quote: the pool's reserves net of every uncollected fee bucket, at the
//! config's trade fee and, where the pool enables it, its creator fee. Leg two is priced
//! on the pool as leg one left it: its vault balances *and* its fee buckets, which leg
//! one's own fee grew — pricing it on the old buckets overstates the reserve by that
//! fee's protocol and fund shares.

use anyhow::{bail, Context, Result};
use cb_core::types::Dex;
use cb_dex::raydium_cpmm as cpmm;
use cb_executor::encode::{pk, programs, to_pubkey};
use cb_executor::pda::associated_token_address;
use cb_executor::rpc::Rpc;
use cb_executor::tx;
use cb_executor::venue::{self, SwapContext, VenueExtra};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let url = flag("--rpc").or_else(|| std::env::var("CRYPTOBOT_RPC_HTTP_URL").ok()).context("--rpc")?;
    let rpc = Rpc::new(url)?;
    let owner = Pubkey::from_str(&flag("--as").context("--as <public address>")?)?;
    let address = Pubkey::from_str(&flag("--pool").context("--pool <address>")?)?;
    let amount: u64 = flag("--amount").and_then(|s| s.parse().ok()).unwrap_or(20_000_000);

    let data = rpc.accounts(&[address]).await?.pop().flatten().context("no pool")?;
    let p = cpmm::decode(&data)?;
    let cfg = rpc.accounts(&[to_pubkey(&p.amm_config)]).await?.pop().flatten().context("no config")?;
    let fee = cpmm::decode_trade_fee_ppm(&cfg)?;
    let creator = cpmm::decode_creator_fee_ppm(&cfg)?;
    let (m0, m1) = (to_pubkey(&p.mint_0), to_pubkey(&p.mint_1));
    let owners = rpc.accounts_full(&[m0, m1]).await?;
    let prog0 = owners[0].as_ref().context("mint 0")?.owner;
    let prog1 = owners[1].as_ref().context("mint 1")?.owner;
    let wsol = pk(programs::WSOL_MINT);
    let sol_is_0 = if m0 == wsol { true } else if m1 == wsol { false } else { bail!("neither side is SOL") };
    println!(
        "pool {address}: trade fee {fee} ppm, creator fee {creator} ppm enabled {} (mode {}), token-2022 {}/{}",
        p.enable_creator_fee, p.creator_fee_on, p.token_2022_0, p.token_2022_1
    );

    let ata0 = associated_token_address(&owner, &m0, &prog0);
    let ata1 = associated_token_address(&owner, &m1, &prog1);
    let wsol_ata = if sol_is_0 { ata0 } else { ata1 };
    let token_ata = if sol_is_0 { ata1 } else { ata0 };
    let mut ixs: Vec<Instruction> = vec![tx::set_compute_limit(1_000_000)];
    ixs.push(tx::create_ata_idempotent(&owner, &ata0, &owner, &m0, &prog0));
    ixs.push(tx::create_ata_idempotent(&owner, &ata1, &owner, &m1, &prog1));
    ixs.push(tx::transfer_lamports(&owner, &wsol_ata, amount));
    ixs.push(tx::sync_native(&wsol_ata));
    let ctx = |input_is_a: bool, amount_in: u64| SwapContext {
        owner,
        pool: address,
        user_source: if input_is_a { ata0 } else { ata1 },
        user_dest: if input_is_a { ata1 } else { ata0 },
        amount_in,
        min_amount_out: 1,
        input_is_a,
        input_token_program: if input_is_a { prog0 } else { prog1 },
        output_token_program: if input_is_a { prog1 } else { prog0 },
        tick_arrays: [Pubkey::default(); 3],
    };
    let extra = VenueExtra::default();
    let (blockhash, _) = rpc.latest_blockhash().await?;
    let (v0, v1) = (to_pubkey(&p.vault_0), to_pubkey(&p.vault_1));
    let sim = |ixs: Vec<Instruction>, watch: Vec<Pubkey>| {
        let rpc = &rpc;
        async move {
            let t = tx::compile_unsigned(&owner, &ixs, blockhash)?;
            let s = rpc.simulate(&t.tx_base64, &watch).await?;
            if let Some(e) = &s.err {
                bail!("simulation failed: {e}\n{}", s.logs.join("\n"));
            }
            if std::env::var_os("CB_LOGS").is_some() {
                for l in s.logs.iter().filter(|l| l.starts_with("Program data:")) {
                    println!("  {l}");
                }
            }
            anyhow::Ok((s.post_token_amounts.iter().map(|a| a.unwrap_or(0)).collect::<Vec<u64>>(), s.post_data))
        }
    };
    let quote = |pool: &cpmm::CpmmPool, input_is_a: bool, a: u64, b: u64, x: u64| -> Option<u128> {
        let s = cpmm::to_pool_state(address.to_bytes(), pool, a, b, fee, creator, 0).ok()?;
        s.leg_for_input(if input_is_a { &p.mint_0 } else { &p.mint_1 })?.quote(u128::from(x))
    };

    // Leg one, priced on the vaults as they stand before it.
    let (pre, _) = sim(ixs.clone(), vec![token_ata, v0, v1]).await?;
    let mut one = ixs.clone();
    one.push(venue::build_swap(Dex::RaydiumCpmm, &ctx(sol_is_0, amount), &data, &extra)?);
    let (post_one, one_data) = sim(one.clone(), vec![token_ata, v0, v1, wsol_ata, address]).await?;
    let got = post_one[0] - pre[0];
    let q = quote(&p, sol_is_0, pre[1], pre[2], amount).context("no quote")?;
    println!("leg 1  spend {amount}: paid {got}, quoted {q} ({:+.4} bps; must be <= 0)", (q as f64 / got as f64 - 1.0) * 1e4);

    // Leg two, priced on the vaults leg one left.
    let back = got * 9 / 10;
    let mut two = one.clone();
    two.push(venue::build_swap(Dex::RaydiumCpmm, &ctx(!sol_is_0, back), &data, &extra)?);
    let (post_two, _) = sim(two, vec![wsol_ata]).await?;
    let got2 = post_two[0] - post_one[3];
    let p_after = cpmm::decode(one_data[4].as_deref().context("pool after leg one")?)?;
    let q2 = quote(&p_after, !sol_is_0, post_one[1], post_one[2], back).context("no quote")?;
    if std::env::var_os("CB_LOGS").is_some() {
        println!("  after leg 1: vaults {} {}, owed {} {} (before: {} {})", post_one[1], post_one[2], p_after.owed_0(), p_after.owed_1(), p.owed_0(), p.owed_1());
    }
    println!("leg 2  spend {back}: paid {got2}, quoted {q2} ({:+.4} bps; must be <= 0)", (q2 as f64 / got2 as f64 - 1.0) * 1e4);
    Ok(())
}
