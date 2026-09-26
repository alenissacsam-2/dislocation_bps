//! Measure a Meteora DAMM v2 pool's swaps against the program's own events.
//!
//! ```text
//! cb-check-damm --as PUBKEY --pool ADDRESS [--rpc URL] [--amount LAMPORTS]
//! ```
//!
//! Simulates, from a public address with `sigVerify` off, spending SOL into the pool and
//! then 90% of what came back, and prints each swap's `EvtSwap2`: which side the fee was
//! taken from, every share of it, and the rate that implies — beside the pool's own base
//! fee, collection mode, fee version and dynamic-fee state — plus what the concentrated
//! curve alone predicts for the post-fee input. This is how the fee rules are learned
//! from the deployed program rather than assumed.

use anyhow::{bail, Context, Result};
use cb_core::types::{Dex, PoolId, PoolMath, PoolState};
use cb_dex::meteora_damm_v2 as damm;
use cb_executor::encode::{pk, programs, to_pubkey};
use cb_executor::pda::associated_token_address;
use cb_executor::rpc::Rpc;
use cb_executor::tx;
use cb_executor::venue::{self, SwapContext, VenueExtra};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

const EMIT_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
const EVT_SWAP2: [u8; 8] = [189, 66, 51, 168, 38, 80, 117, 153];

struct Evt {
    direction: u8,
    mode: u8,
    included_in: u64,
    excluded_in: u64,
    out: u64,
    claiming: u64,
    protocol: u64,
    compounding: u64,
    referral: u64,
}

async fn events(url: &str, tx_base64: &str) -> Result<Vec<Evt>> {
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"simulateTransaction","params":[tx_base64,
        {"encoding":"base64","sigVerify":false,"replaceRecentBlockhash":true,"innerInstructions":true,"commitment":"processed"}]});
    let v: serde_json::Value = reqwest::Client::new().post(url).json(&body).send().await?.json().await?;
    if let Some(e) = v["result"]["value"]["err"].as_object() {
        bail!("simulation failed: {e:?}\n{}", v["result"]["value"]["logs"]);
    }
    let mut out = Vec::new();
    for g in v["result"]["value"]["innerInstructions"].as_array().cloned().unwrap_or_default() {
        for ix in g["instructions"].as_array().cloned().unwrap_or_default() {
            let Some(d) = ix["data"].as_str().and_then(|s| bs58::decode(s).into_vec().ok()) else { continue };
            if d.len() < 196 || d[..8] != EMIT_TAG || d[8..16] != EVT_SWAP2 {
                continue;
            }
            let u = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().expect("u64"));
            out.push(Evt {
                direction: d[48],
                mode: d[49],
                included_in: u(68),
                excluded_in: u(76),
                out: u(92),
                claiming: u(116),
                protocol: u(124),
                compounding: u(132),
                referral: u(140),
            });
        }
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let url = flag("--rpc").or_else(|| std::env::var("CRYPTOBOT_RPC_HTTP_URL").ok()).context("--rpc")?;
    let rpc = Rpc::new(url.clone())?;
    let owner = Pubkey::from_str(&flag("--as").context("--as <public address>")?)?;
    let address = Pubkey::from_str(&flag("--pool").context("--pool <address>")?)?;
    let amount: u64 = flag("--amount").and_then(|s| s.parse().ok()).unwrap_or(20_000_000);

    let data = rpc.accounts(&[address]).await?.pop().flatten().context("no pool")?;
    let p = damm::decode_layout(&data)?;
    let (mint_a, mint_b) = (to_pubkey(&p.mint_a), to_pubkey(&p.mint_b));
    let owners = rpc.accounts_full(&[mint_a, mint_b]).await?;
    let prog_a = owners[0].as_ref().context("mint a")?.owner;
    let prog_b = owners[1].as_ref().context("mint b")?.owner;
    let wsol = pk(programs::WSOL_MINT);
    let sol_is_a = if mint_a == wsol {
        true
    } else if mint_b == wsol {
        false
    } else {
        bail!("neither side is SOL");
    };
    println!("pool {address}: base fee {} ppm (cliff {}), schedule {}, protocol {}%, referral {}%, compounding {} bps, \
              collect mode {}, fee version {}, status {}, token flags {}/{}",
        p.cliff_fee_numerator / 1000, p.cliff_fee_numerator, p.has_fee_schedule, p.protocol_fee_percent,
        p.referral_fee_percent, p.compounding_fee_bps, p.collect_fee_mode, p.fee_version, p.pool_status,
        p.token_a_flag, p.token_b_flag);
    if let Some(d) = p.dynamic_fee {
        println!("  dynamic: bin_step {} control {} max_vol {} filter {} decay {} reduction {} vol_acc {} vol_ref {} last_update {}",
            d.bin_step, d.variable_fee_control, d.max_volatility_accumulator, d.filter_period, d.decay_period,
            d.reduction_factor, d.volatility_accumulator, d.volatility_reference, d.last_update_timestamp);
    }

    let ata_a = associated_token_address(&owner, &mint_a, &prog_a);
    let ata_b = associated_token_address(&owner, &mint_b, &prog_b);
    let wsol_ata = if sol_is_a { ata_a } else { ata_b };
    let mut ixs: Vec<Instruction> = vec![tx::set_compute_limit(1_000_000)];
    ixs.push(tx::create_ata_idempotent(&owner, &ata_a, &owner, &mint_a, &prog_a));
    ixs.push(tx::create_ata_idempotent(&owner, &ata_b, &owner, &mint_b, &prog_b));
    ixs.push(tx::transfer_lamports(&owner, &wsol_ata, amount));
    ixs.push(tx::sync_native(&wsol_ata));
    let ctx = |input_is_a: bool, amount_in: u64| SwapContext {
        owner,
        pool: address,
        user_source: if input_is_a { ata_a } else { ata_b },
        user_dest: if input_is_a { ata_b } else { ata_a },
        amount_in,
        min_amount_out: 1,
        input_is_a,
        input_token_program: if input_is_a { prog_a } else { prog_b },
        output_token_program: if input_is_a { prog_b } else { prog_a },
        tick_arrays: [Pubkey::default(); 3],
    };
    let extra = VenueExtra::default();
    let (blockhash, _) = rpc.latest_blockhash().await?;

    // Leg one: SOL in.
    ixs.push(venue::build_swap(Dex::MeteoraDammV2, &ctx(sol_is_a, amount), &data, &extra)?);
    let one = events(&url, &tx::compile_unsigned(&owner, &ixs, blockhash)?.tx_base64).await?;
    let e1 = one.first().context("no EvtSwap2 from leg one")?;
    // Leg two: 90% of the token back.
    let back = e1.out * 9 / 10;
    ixs.push(venue::build_swap(Dex::MeteoraDammV2, &ctx(!sol_is_a, back), &data, &extra)?);
    let two = events(&url, &tx::compile_unsigned(&owner, &ixs, blockhash)?.tx_base64).await?;

    let curve = |input_is_a: bool, x: u64| -> Option<u128> {
        let s = PoolState {
            id: PoolId(address.to_bytes()),
            dex: Dex::MeteoraDammV2,
            mint_a: p.mint_a,
            mint_b: p.mint_b,
            math: PoolMath::Concentrated {
                liquidity: p.liquidity,
                sqrt_price_x64: p.sqrt_price_x64,
                sqrt_lo_x64: p.sqrt_min_price_x64,
                sqrt_hi_x64: p.sqrt_max_price_x64,
            },
            fee_ppm: 0,
            slot: 0,
        };
        let input = if input_is_a { p.mint_a } else { p.mint_b };
        s.leg_for_input(&input)?.quote(u128::from(x))
    };
    for (leg, e) in [("leg 1", e1), ("leg 2", two.last().context("no EvtSwap2 from leg two")?)] {
        let fees = e.claiming + e.protocol + e.compounding + e.referral;
        let on_input = e.included_in > e.excluded_in;
        let base = if on_input { e.included_in } else { e.out + fees };
        println!(
            "{leg}: direction {} mode {} | in {} (after fee {}) out {} | fees claiming {} protocol {} compounding {} referral {} \
             = {fees}, taken from the {} side, {:.2} ppm of it",
            e.direction, e.mode, e.included_in, e.excluded_in, e.out, e.claiming, e.protocol, e.compounding, e.referral,
            if on_input { "input" } else { "output" },
            fees as f64 / base as f64 * 1e6
        );
        if leg == "leg 1" {
            // The bot's own quote, fee and all, from the account read before the swap.
            let model = damm::to_pool_state(address.to_bytes(), &data, 0)
                .ok()
                .and_then(|st| st.leg_for_input(if sol_is_a { &p.mint_a } else { &p.mint_b }).and_then(|l| l.quote(u128::from(amount))));
            match model {
                Some(m) => println!("        the bot's quote {m} vs paid {} ({:+.4} bps; must be <= 0)", e.out, (m as f64 / e.out as f64 - 1.0) * 1e4),
                None => println!("        the bot refuses to quote this pool: {:?}", damm::to_pool_state(address.to_bytes(), &data, 0).err()),
            }
            let input_is_a = sol_is_a;
            if let Some(gross) = curve(input_is_a, e.excluded_in) {
                let paid_gross = if on_input { e.out } else { e.out + fees };
                println!("        curve on the post-fee input: {gross} vs the program's gross {paid_gross} ({:+.4} bps)",
                    (gross as f64 / paid_gross as f64 - 1.0) * 1e4);
            }
        }
    }
    Ok(())
}
