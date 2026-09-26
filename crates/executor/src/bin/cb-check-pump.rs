//! Check PumpSwap pricing and encoding against the program, on live pools.
//!
//! ```text
//! cb-check-pump --as PUBKEY --pool ADDRESS [--rpc URL] [--amount LAMPORTS]
//! ```
//!
//! Simulates, from a public address with `sigVerify` off (no key, nothing sent), a
//! purchase with SOL and then a sale of what it bought, and compares each output with
//! what the bot's quote predicts from the same accounts. It also prints both fee
//! hypotheses — the market-cap tier and the flat rate — because which one a pool pays
//! depends on whether PumpSwap counts it as a pump.fun pool, and the simulation is what
//! settles that.

use anyhow::{bail, Context, Result};
use cb_core::types::Pubkey32;
use cb_dex::pumpswap::{self as ps, PumpPool};
use cb_executor::encode::{pk, programs, to_pubkey};
use cb_executor::pda::associated_token_address;
use cb_executor::rpc::Rpc;
use cb_executor::venue::pumpswap::{self as venue, PumpFeeRecipients};
use cb_executor::venue::SwapContext;
use cb_executor::tx;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

fn amount_of(data: &[u8]) -> u64 {
    u64::from_le_bytes(data[64..72].try_into().expect("token account"))
}

struct Live {
    pool: PumpPool,
    base_reserve: u64,
    quote_reserve: u64,
    supply: u64,
    base_program: Pubkey,
    quote_program: Pubkey,
}

async fn read(rpc: &Rpc, key: Pubkey) -> Result<Live> {
    let data = rpc.accounts(&[key]).await?.pop().flatten().context("no pool account")?;
    let pool = ps::decode_pool(&data)?;
    let got = rpc
        .accounts_full(&[
            to_pubkey(&pool.base_vault),
            to_pubkey(&pool.quote_vault),
            to_pubkey(&pool.base_mint),
            to_pubkey(&pool.quote_mint),
        ])
        .await?;
    let get = |i: usize| got[i].as_ref().context("an account is missing");
    Ok(Live {
        base_reserve: amount_of(&get(0)?.data),
        quote_reserve: amount_of(&get(1)?.data),
        supply: u64::from_le_bytes(get(2)?.data[36..44].try_into()?),
        base_program: get(2)?.owner,
        quote_program: get(3)?.owner,
        pool,
    })
}

fn quote(live: &Live, address: Pubkey, fee_bps: u64, spend_base: bool, amount: u64) -> Result<u128> {
    let state = ps::to_pool_state(address.to_bytes(), &live.pool, live.base_reserve, live.quote_reserve, fee_bps, 0)?;
    let input: Pubkey32 = if spend_base { live.pool.base_mint } else { live.pool.quote_mint };
    let leg = state.leg_for_input(&input).context("no leg")?;
    leg.quote(u128::from(amount)).context("no quote")
}

/// Simulate with inner instructions and print every PumpSwap trade event found in them.
///
/// PumpSwap emits its events by invoking itself (Anchor `emit_cpi`), so they are in the
/// inner instruction data, not in the logs: an 8-byte emit tag, the event's own
/// 8-byte discriminator, then its fields.
async fn events(url: &str, tx_base64: &str, supply: u64, pool: &PumpPool, cfg: &ps::FeeConfig, pump: bool) -> Result<()> {
    const EMIT_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
    const BUY: [u8; 8] = [103, 244, 82, 31, 44, 245, 119, 119];
    const SELL: [u8; 8] = [62, 47, 55, 10, 165, 3, 220, 42];
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"simulateTransaction","params":[tx_base64,
        {"encoding":"base64","sigVerify":false,"replaceRecentBlockhash":true,"innerInstructions":true,"commitment":"processed"}]});
    let v: serde_json::Value = reqwest::Client::new().post(url).json(&body).send().await?.json().await?;
    let groups = v["result"]["value"]["innerInstructions"].as_array().cloned().unwrap_or_default();
    for g in groups {
        for ix in g["instructions"].as_array().cloned().unwrap_or_default() {
            let Some(d58) = ix["data"].as_str() else { continue };
            let Ok(d) = bs58::decode(d58).into_vec() else { continue };
            if d.len() < 16 || d[..8] != EMIT_TAG {
                continue;
            }
            let u = |i: usize| u64::from_le_bytes(d[16 + 8 * i..24 + 8 * i].try_into().expect("u64"));
            let name = if d[8..16] == BUY {
                "BUY"
            } else if d[8..16] == SELL {
                "SELL"
            } else {
                continue;
            };
            // Both events: 13 fixed u64 after the timestamp, then 7 pubkeys, then the
            // creator's bps and fee.
            let creator_at = 16 + 8 * 14 + 32 * 7;
            let cu = |i: usize| u64::from_le_bytes(d[creator_at + 8 * i..creator_at + 8 * i + 8].try_into().expect("u64"));
            let cap = ps::market_cap(supply, u(5), pool.effective_quote(u(6)));
            let model_bps = cfg.fees(pump, cap, false).total_bps();
            let charged = u(8) + u(10) + cu(0);
            // The model's quote at the reserves the program itself reports.
            let state = ps::to_pool_state([0u8; 32], pool, u(5), u(6), model_bps, 0)?;
            let (input_mint, spent, received) = if name == "BUY" {
                (pool.quote_mint, u(7), u(1))
            } else {
                (pool.base_mint, u(1), u(13))
            };
            let model = state.leg_for_input(&input_mint).and_then(|l| l.quote(u128::from(spent))).unwrap_or(0);
            println!(
                "        {name} at the event's reserves: market cap {:.1} SOL, charged {charged} bps, model {model_bps} bps; \
                 received {received}, model {model} ({:+.4} bps)",
                cap as f64 / 1e9,
                (model as f64 / received as f64 - 1.0) * 1e4
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let url = flag("--rpc").or_else(|| std::env::var("CRYPTOBOT_RPC_HTTP_URL").ok()).context("--rpc or CRYPTOBOT_RPC_HTTP_URL")?;
    let rpc = Rpc::new(url.clone())?;
    let owner = Pubkey::from_str(&flag("--as").context("--as <public address>")?)?;
    let address = Pubkey::from_str(&flag("--pool").context("--pool <address>")?)?;
    let amount: u64 = flag("--amount").and_then(|s| s.parse().ok()).unwrap_or(20_000_000);

    let global = rpc.accounts(&[pk(ps::GLOBAL_CONFIG)]).await?.pop().flatten().context("no global config")?;
    let global = ps::decode_global_config(&global)?;
    let fee_cfg = rpc.accounts(&[venue::fee_config()]).await?.pop().flatten().context("no fee config")?;
    let fee_cfg = ps::decode_fee_config(&fee_cfg)?;
    let recipients = PumpFeeRecipients {
        protocol: to_pubkey(&global.protocol_fee_recipients[0]),
        buyback: to_pubkey(&global.buyback_fee_recipients[0]),
    };

    let live = read(&rpc, address).await?;
    let wsol = pk(programs::WSOL_MINT);
    let (base_mint, quote_mint) = (to_pubkey(&live.pool.base_mint), to_pubkey(&live.pool.quote_mint));
    let spend_base_first = if quote_mint == wsol {
        false
    } else if base_mint == wsol {
        true
    } else {
        bail!("neither side of this pool is SOL; this tool funds only from SOL");
    };
    let stable = false;
    let is_pump = to_pubkey(&live.pool.creator)
        == Pubkey::find_program_address(&[b"pool-authority", base_mint.as_ref()], &pk("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P")).0;
    let cap = ps::market_cap(live.supply, live.base_reserve, live.quote_reserve);
    let tier = fee_cfg.fees(true, cap, stable).total_bps();
    let flat = fee_cfg.fees(false, cap, stable).total_bps();
    let pump_authority = Pubkey::find_program_address(
        &[b"pool-authority", base_mint.as_ref()],
        &pk("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"),
    )
    .0;
    println!("pool      {address}  index {}  creator {}", live.pool.index, to_pubkey(&live.pool.creator));
    println!("          pump.fun pool-authority for this mint: {pump_authority} (creator matches: {})",
        to_pubkey(&live.pool.creator) == pump_authority);
    println!("          mayhem {} cashback {} coin_creator set {}", live.pool.is_mayhem_mode, live.pool.is_cashback_coin,
        live.pool.coin_creator != [0u8; 32]);
    let v2 = venue::pool_v2(&base_mint);
    match rpc.accounts_full(&[v2]).await?.pop().flatten() {
        Some(a) => {
            let d = &a.data;
            let words: Vec<String> = d.chunks(8).skip(1).take(12).map(|c| {
                let mut b = [0u8; 8];
                b[..c.len()].copy_from_slice(c);
                u64::from_le_bytes(b).to_string()
            }).collect();
            println!("pool_v2   {v2}: {} bytes, owner {}; u64 words after the discriminator: {}", d.len(), a.owner, words.join(" "));
        }
        None => println!("pool_v2   {v2}: does not exist"),
    }
    println!("reserves  base {} quote {}  supply {}  market cap {:.2} SOL", live.base_reserve, live.quote_reserve, live.supply, cap as f64 / 1e9);
    println!("fees      tier {tier} bps, flat {flat} bps");

    // The buyer's accounts, created idempotently inside the probe.
    let base_ata = associated_token_address(&owner, &base_mint, &live.base_program);
    let quote_ata = associated_token_address(&owner, &quote_mint, &live.quote_program);
    let wsol_ata = if quote_mint == wsol { quote_ata } else { base_ata };
    let token_ata = if quote_mint == wsol { base_ata } else { quote_ata };
    let mut setup: Vec<Instruction> = vec![tx::set_compute_limit(1_000_000)];
    setup.push(tx::create_ata_idempotent(&owner, &base_ata, &owner, &base_mint, &live.base_program));
    setup.push(tx::create_ata_idempotent(&owner, &quote_ata, &owner, &quote_mint, &live.quote_program));
    setup.push(tx::transfer_lamports(&owner, &wsol_ata, amount));
    setup.push(tx::sync_native(&wsol_ata));
    let has_uva = rpc.accounts(&[venue::user_volume_accumulator(&owner)]).await?.pop().flatten().is_some();
    if !has_uva {
        setup.push(venue::init_user_volume_accumulator(&owner));
    }
    println!("          user volume accumulator exists: {has_uva}");

    let ctx = |spend_base: bool, amount_in: u64| SwapContext {
        owner,
        pool: address,
        user_source: if spend_base { base_ata } else { quote_ata },
        user_dest: if spend_base { quote_ata } else { base_ata },
        amount_in,
        min_amount_out: 1,
        input_is_a: spend_base,
        input_token_program: if spend_base { live.base_program } else { live.quote_program },
        output_token_program: if spend_base { live.quote_program } else { live.base_program },
        tick_arrays: [Pubkey::default(); 3],
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
            anyhow::Ok(s.post_token_amounts.iter().map(|a| a.unwrap_or(0)).collect::<Vec<u64>>())
        }
    };

    // Leg one: SOL into the token.
    let before = sim(setup.clone(), vec![token_ata]).await?[0];
    let mut one = setup.clone();
    one.push(venue::swap(&ctx(spend_base_first, amount), &{
        rpc.accounts(&[address]).await?.pop().flatten().context("pool")?
    }, Some(recipients))?);
    let after_one = sim(one.clone(), vec![token_ata, to_pubkey(&live.pool.base_vault), to_pubkey(&live.pool.quote_vault)]).await?;
    let got_one = after_one[0] - before;
    events(&url, &tx::compile_unsigned(&owner, &one, blockhash)?.tx_base64, live.supply, &live.pool, &fee_cfg, is_pump).await?;
    let q_tier = quote(&live, address, tier, spend_base_first, amount)?;
    let q_flat = quote(&live, address, flat, spend_base_first, amount)?;
    println!("\nleg 1   spend {amount} ({})", if spend_base_first { "sell" } else { "buy_exact_quote_in" });
    println!("        paid {got_one}   quote@tier {q_tier} ({:+.2} bps)   quote@flat {q_flat} ({:+.2} bps)",
        (q_tier as f64 / got_one as f64 - 1.0) * 1e4, (q_flat as f64 / got_one as f64 - 1.0) * 1e4);

    // Leg two: sell back most of it, priced on the state leg one left behind.
    let back = got_one * 9 / 10;
    let after = Live { base_reserve: after_one[1], quote_reserve: after_one[2], ..live };
    let cap2 = ps::market_cap(after.supply, after.base_reserve, after.quote_reserve);
    let tier2 = fee_cfg.fees(true, cap2, stable).total_bps();
    let mut two = one.clone();
    two.push(venue::swap(&ctx(!spend_base_first, back), &{
        rpc.accounts(&[address]).await?.pop().flatten().context("pool")?
    }, Some(recipients))?);
    events(&url, &tx::compile_unsigned(&owner, &two, blockhash)?.tx_base64, live.supply, &live.pool, &fee_cfg, is_pump).await?;
    let wsol_before = sim(one, vec![wsol_ata]).await?[0];
    let wsol_after = sim(two, vec![wsol_ata]).await?[0];
    let got_two = wsol_after - wsol_before;
    let q2_tier = quote(&after, address, tier2, !spend_base_first, back)?;
    let q2_flat = quote(&after, address, flat, !spend_base_first, back)?;
    println!("leg 2   spend {back} ({})", if spend_base_first { "buy_exact_quote_in" } else { "sell" });
    println!("        paid {got_two}   quote@tier {q2_tier} ({:+.2} bps)   quote@flat {q2_flat} ({:+.2} bps)",
        (q2_tier as f64 / got_two as f64 - 1.0) * 1e4, (q2_flat as f64 / got_two as f64 - 1.0) * 1e4);
    println!("\n(positive bps = the quote promised more than the program paid; must be <= 0)");
    Ok(())
}
