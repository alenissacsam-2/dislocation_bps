//! Check the swap encoders against live mainnet.
//!
//! ```text
//! cb-verify-encode [--rpc URL] [--pools PATH] [--as PUBKEY] [--limit N] [--verbose] [--raw]
//! ```
//!
//! This is a **separate binary from `cb-bot` on purpose**. `cb-bot` links neither
//! `cb-executor` nor `cb-wallet` nor `solana-sdk`, which is the load-bearing half of
//! this project's paper-mode guarantee: that binary contains no path to a signature
//! whatever its config says. Adding a verification flag to it would have quietly
//! deleted that property to save a file.
//!
//! **No key is involved anywhere.** Simulation runs with `sigVerify` off, so a
//! placeholder signature is as good as a real one, and `--as` takes a *public* address.
//! Verification therefore cannot spend or expose anything, and a diagnostic never puts
//! the operator's wallet in its own path.
//!
//! An earlier version signed with a freshly generated throwaway key. That does not work,
//! and the failure is worth recording: a keypair that has never been funded has no
//! account on Solana at all, and a fee payer with no account is rejected by the runtime
//! before the program is loaded. Every pool came back `AccountNotFound` with no logs,
//! which looks exactly like a broken encoder and is not one.

use anyhow::{Context, Result};
use cb_core::types::Dex;
use cb_executor::encode::{pk, programs, to_pubkey};
use cb_executor::pda::{associated_token_address, orca_oracle};
use cb_executor::rpc::Rpc;
use cb_executor::venue::raydium::BitmapPolicy;
use cb_executor::venue::{SwapContext, VenueExtra};
use cb_executor::verify::{
    classify, orca_tick_array_header, raydium_tick_array_header, token_account_mint, Check,
    PoolReport, Verdict, ORCA_TICK_ARRAY_LEN,
};
use cb_executor::{ticks, tx, venue};
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
struct Registry {
    pools: Vec<RawPool>,
}

#[derive(Debug, Deserialize)]
struct RawPool {
    address: String,
    dex: String,
    label: String,
}

/// A trivially small trade. Big enough that the programs do not reject it as zero,
/// small enough to be meaningless if anything ever went wrong.
const PROBE_AMOUNT: u64 = 1_000;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let verbose = args.iter().any(|a| a == "--verbose");

    let rpc_url = flag("--rpc")
        .or_else(|| std::env::var("CRYPTOBOT_RPC_HTTP_URL").ok())
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());
    let pools_path = flag("--pools").unwrap_or_else(|| "crates/bot/pools.json".to_string());
    let limit: usize = flag("--limit").and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);

    let raw = std::fs::read_to_string(&pools_path)
        .with_context(|| format!("could not read the pool registry at {pools_path}"))?;
    let registry: Registry =
        serde_json::from_str(&raw).context("pools.json is not the registry")?;

    let rpc = Rpc::new(&rpc_url)?;

    // The address to simulate as. Public only: simulation runs with sigVerify off, so
    // no key is needed and none is read. Without one the swap check cannot run at all —
    // a payer that has never been funded does not exist on Solana, and the runtime
    // rejects the transaction before the program is ever loaded.
    let simulate_as = match flag("--as") {
        Some(s) => match Pubkey::from_str(&s) {
            Ok(k) => Some(k),
            Err(_) => anyhow::bail!("--as {s} is not a public key"),
        },
        None => None,
    };

    println!("cb-verify-encode");
    println!("  rpc     {rpc_url}");
    println!("  pools   {pools_path}");
    match simulate_as {
        Some(k) => println!("  as      {k} (public address only; no key is read)"),
        None => {
            println!("  as      <none> - address checks only");
            println!("          Pass --as <a funded address holding these mints> to also");
            println!("          check the instruction. Only the public address is needed.");
        }
    }
    println!();

    let mut reports = Vec::new();
    for raw in registry.pools.iter().filter(|p| supported(&p.dex).is_some()).take(limit) {
        let dex = supported(&raw.dex).expect("filtered above");
        match check_pool(&rpc, simulate_as, raw, dex).await {
            Ok(r) => reports.push(r),
            Err(e) => reports.push(PoolReport {
                address: raw.address.clone(),
                label: raw.label.clone(),
                dex,
                checks: vec![Check {
                    name: "fetch",
                    verdict: Verdict::Inconclusive,
                    detail: format!("{e}"),
                }],
            }),
        }
        print_last(&reports, verbose);
    }

    summarise(&reports);
    Ok(())
}

fn supported(dex: &str) -> Option<Dex> {
    match dex {
        "orca_whirlpool" => Some(Dex::OrcaWhirlpool),
        "raydium_clmm" => Some(Dex::RaydiumClmm),
        "raydium_v4" => Some(Dex::RaydiumAmmV4),
        "meteora_dlmm" => Some(Dex::MeteoraDlmm),
        _ => None,
    }
}

async fn check_pool(
    rpc: &Rpc,
    simulate_as: Option<Pubkey>,
    raw: &RawPool,
    dex: Dex,
) -> Result<PoolReport> {
    let pool = Pubkey::from_str(&raw.address).context("pool address is not a pubkey")?;
    let mut checks = Vec::new();

    let Some(pool_data) = rpc.accounts(&[pool]).await?.into_iter().next().flatten() else {
        checks.push(Check {
            name: "pool",
            verdict: Verdict::Fail,
            detail: "the pool account does not exist".into(),
        });
        return Ok(PoolReport {
            address: raw.address.clone(),
            label: raw.label.clone(),
            dex,
            checks,
        });
    };

    // Constant-product venues share none of what follows — no tick, no spacing, no
    // oracle, no arrays to find — so they take their own much shorter route.
    if dex == Dex::RaydiumAmmV4 {
        return check_v4(rpc, simulate_as, raw, pool, &pool_data, checks).await;
    }
    // And a binned venue shares even less: no tick, no spacing, and an oracle the pool
    // account names rather than one that is derived.
    if dex == Dex::MeteoraDlmm {
        return check_dlmm(rpc, simulate_as, raw, pool, &pool_data, checks).await;
    }

    let (mint_a, mint_b, vault_a, vault_b, tick, spacing, liquidity, program) = match dex {
        Dex::OrcaWhirlpool => {
            let w = cb_dex::orca_whirlpool::decode(&pool_data)?;
            (
                w.mint_a,
                w.mint_b,
                w.vault_a,
                w.vault_b,
                w.tick_current,
                w.tick_spacing,
                w.liquidity,
                pk(cb_dex::orca_whirlpool::PROGRAM_ID),
            )
        }
        _ => {
            let p = cb_dex::raydium_clmm::decode(&pool_data)?;
            (
                p.mint_0,
                p.mint_1,
                p.vault_0,
                p.vault_1,
                p.tick_current,
                p.tick_spacing,
                p.liquidity,
                pk(cb_dex::raydium_clmm::PROGRAM_ID),
            )
        }
    };

    // The direction the probe swap will take: spending token A pushes the price down.
    let falling = true;

    let oracle = orca_oracle(&pool, &program);
    let fetched = rpc.accounts_full(&[to_pubkey(&vault_a), to_pubkey(&vault_b), oracle]).await?;

    // ---- 1. The vaults must be token accounts holding the pool's own mints. ----
    for (i, want_mint) in [mint_a, mint_b].iter().enumerate() {
        let side = if i == 0 { "vault_a" } else { "vault_b" };
        checks.push(match &fetched[i] {
            None => Check {
                name: "vault",
                verdict: Verdict::Fail,
                detail: format!("{side} does not exist - the vault offset is wrong"),
            },
            Some(acc) => match token_account_mint(&acc.data) {
                Err(e) => {
                    Check { name: "vault", verdict: Verdict::Fail, detail: format!("{side}: {e}") }
                }
                Ok(m) if m == *want_mint => Check {
                    name: "vault",
                    verdict: Verdict::Pass,
                    detail: format!("{side} is a token account holding the pool's own mint"),
                },
                Ok(m) => Check {
                    name: "vault",
                    verdict: Verdict::Fail,
                    detail: format!(
                        "{side} holds {} but the pool says {}",
                        bs58::encode(m).into_string(),
                        bs58::encode(want_mint).into_string()
                    ),
                },
            },
        });
    }

    // ---- 2. The derivation must land on real arrays belonging to this pool. ----
    //
    // Not "the array containing the current tick exists" — that was the first version
    // of this check and it failed 23 of 48 Raydium pools for a reason that had nothing
    // to do with the derivation. A tick array holds position boundaries, so the one
    // containing the current price is created only if some position starts or ends
    // inside it. The right question is whether the swept addresses resolve to real
    // arrays that name this pool.
    // Both directions. A swap only ever walks one way, but the question here is whether
    // the *derivation* lands on real arrays, and a pool whose liquidity all sits above
    // the current tick has nothing below it to find. Sweeping one way would report that
    // pool as a derivation failure, which is what the first version of this check did.
    let chosen = ticks::resolve(rpc, dex, &pool, &program, tick, spacing, falling).await?;
    let chosen = if chosen.found > 0 {
        chosen
    } else {
        ticks::resolve(rpc, dex, &pool, &program, tick, spacing, !falling).await?
    };
    checks.push(if chosen.found == 0 {
        // No arrays anywhere, in either direction, across the whole sweep. A Raydium or
        // Orca pool cannot be swapped through without one, so this is a fact about the
        // pool rather than about the encoder — and calling it a FAIL would blame the
        // derivation for a pool that nothing could trade. Measured on this registry: 21
        // of 48 Raydium CLMM pools are in this state while reporting non-zero
        // liquidity, which is worth knowing for reasons beyond execution.
        Check {
            name: "tick_array",
            verdict: Verdict::Inconclusive,
            detail: format!(
                "the pool has no tick arrays at any of the {} addresses swept in both \
                 directions, so nothing can swap through it whatever the encoding (tick \
                 {tick}, spacing {spacing}, liquidity {liquidity})",
                ticks::SWEEP_WIDTH * 2
            ),
        }
    } else {
        // Confirm one of them declares the start index we derived for it, which is what
        // actually tests the seed scheme and the floor division.
        let first_start = chosen.starts[0];
        let header = rpc.accounts_full(&[chosen.arrays[0]]).await?.into_iter().next().flatten();
        match header {
            Some(acc) => {
                let read = match dex {
                    Dex::OrcaWhirlpool => {
                        orca_tick_array_header(&acc.data).map(|h| (h.start_tick_index, h.whirlpool))
                    }
                    _ => raydium_tick_array_header(&acc.data).map(|h| (h.start_tick_index, h.pool)),
                };
                match read {
                    Err(e) if dex == Dex::OrcaWhirlpool && acc.data.len() != ORCA_TICK_ARRAY_LEN => {
                        Check {
                            name: "tick_array",
                            verdict: Verdict::Inconclusive,
                            detail: format!(
                                "{} of {} swept addresses are real arrays owned by the pool's \
                                 program, but the nearest is {} bytes rather than the \
                                 {ORCA_TICK_ARRAY_LEN}-byte fixed layout, so its header cannot \
                                 be read here ({e})",
                                chosen.found,
                                ticks::SWEEP_WIDTH,
                                acc.data.len()
                            ),
                        }
                    }
                    Err(e) => Check {
                        name: "tick_array",
                        verdict: Verdict::Fail,
                        detail: e.to_string(),
                    },
                    Ok((got_start, got_pool)) if got_start == first_start
                        && got_pool == pool.to_bytes() =>
                    {
                        Check {
                            name: "tick_array",
                            verdict: Verdict::Pass,
                            detail: format!(
                                "{} of {} swept addresses are live; the nearest declares start \
                                 {got_start} and names this pool{}",
                                chosen.found,
                                ticks::SWEEP_WIDTH,
                                if chosen.current_exists {
                                    ""
                                } else {
                                    " (the array containing the current tick was never created, \
                                      which is normal)"
                                }
                            ),
                        }
                    }
                    Ok((got_start, got_pool)) => Check {
                        name: "tick_array",
                        verdict: Verdict::Fail,
                        detail: format!(
                            "derived start {first_start} but the array declares {got_start}; \
                             names this pool: {}",
                            got_pool == pool.to_bytes()
                        ),
                    },
                }
            }
            None => Check {
                name: "tick_array",
                verdict: Verdict::Fail,
                detail: "an array reported live vanished between two calls".into(),
            },
        }
    });

    // ---- 3. Orca's oracle. ----
    //
    // A classic Whirlpool's oracle PDA is a placeholder the program never initialises,
    // so its absence is normal and is evidence for nothing. Only a *wrongly owned*
    // account at that address would be.
    if dex == Dex::OrcaWhirlpool {
        checks.push(match &fetched[2] {
            Some(acc) if acc.owner == program => Check {
                name: "oracle",
                verdict: Verdict::Pass,
                detail: "the derived oracle exists and belongs to the whirlpool program".into(),
            },
            Some(acc) => Check {
                name: "oracle",
                verdict: Verdict::Fail,
                detail: format!(
                    "the derived oracle is owned by {}, not the pool's program",
                    acc.owner
                ),
            },
            None => Check {
                name: "oracle",
                verdict: Verdict::Inconclusive,
                detail: "the derived oracle does not exist, which is normal for a pool without \
                         adaptive fees - the program treats it as a placeholder"
                    .into(),
            },
        });
    }

    // ---- 4. Simulate the instruction itself. ----
    //
    // This is the only check that needs an address which actually exists and holds the
    // pool's mints. Without one the runtime rejects the transaction before running it,
    // which says nothing about the encoder, so the check is skipped rather than failed.
    // A pool with no tick arrays cannot be swapped through by anyone, so simulating one
    // measures the pool rather than the encoder. Every such pool failed with "an account
    // belongs to the wrong program" — correct, and about the empty array address rather
    // than the account order. Skipping keeps the swap column meaning what it claims.
    if chosen.found == 0 {
        checks.push(Check {
            name: "swap",
            verdict: Verdict::Inconclusive,
            detail: "skipped: this pool has no tick arrays, so a swap through it cannot be \
                     built by anyone and simulating one says nothing about the encoder"
                .into(),
        });
        return Ok(PoolReport {
            address: raw.address.clone(),
            label: raw.label.clone(),
            dex,
            checks,
        });
    }

    let Some(owner) = simulate_as else {
        checks.push(Check {
            name: "swap",
            verdict: Verdict::Inconclusive,
            detail: "skipped: pass --as <funded address> to check the instruction".into(),
        });
        return Ok(PoolReport {
            address: raw.address.clone(),
            label: raw.label.clone(),
            dex,
            checks,
        });
    };

    let token_program = pk(programs::SPL_TOKEN);
    let ctx = SwapContext {
        owner,
        pool,
        user_source: associated_token_address(&owner, &to_pubkey(&mint_a), &token_program),
        user_dest: associated_token_address(&owner, &to_pubkey(&mint_b), &token_program),
        amount_in: PROBE_AMOUNT,
        min_amount_out: 1,
        input_is_a: true,
        input_token_program: token_program,
        output_token_program: token_program,
        tick_arrays: chosen.arrays,
    };

    let policies: &[Option<BitmapPolicy>] = if dex == Dex::RaydiumClmm {
        &[Some(BitmapPolicy::Include), Some(BitmapPolicy::Omit)]
    } else {
        &[None]
    };

    for policy in policies {
        let extra =
            VenueExtra { token_program, bitmap_policy: policy.unwrap_or(BitmapPolicy::Include) };
        let name = match policy {
            Some(BitmapPolicy::Include) => "swap(bitmap)",
            Some(BitmapPolicy::Omit) => "swap(no bitmap)",
            Some(BitmapPolicy::Auto) => "swap(bitmap if needed)",
            None => "swap",
        };
        let ix = match venue::build_swap(dex, &ctx, &pool_data, &extra) {
            Ok(i) => i,
            Err(e) => {
                checks.push(Check { name, verdict: Verdict::Fail, detail: e.to_string() });
                continue;
            }
        };
        // Create both token accounts inside the probe.
        //
        // Without this the program stops at position 3 of 11 — the simulating address's
        // own token account, which it does not hold — and everything after it stays
        // unchecked. The idempotent variant is a no-op when the account already exists,
        // so this costs nothing for a funded wallet and makes the account order testable
        // for one that is not. It needs the address to hold enough SOL for rent
        // (~0.00204 per account); simulation charges it the same as a real run would.
        let mut probe = vec![tx::set_compute_limit(600_000)];
        for mint in [&mint_a, &mint_b] {
            let m = to_pubkey(mint);
            let ata = associated_token_address(&owner, &m, &token_program);
            probe.push(tx::create_ata_idempotent(&owner, &ata, &owner, &m, &token_program));
        }
        probe.push(ix);

        let (blockhash, _) = rpc.latest_blockhash().await?;
        // Unsigned: a placeholder signature, because simulation does not check one and
        // verification must never need a key.
        let compiled = match tx::compile_unsigned(&owner, &probe, blockhash) {
            Ok(a) => a,
            Err(e) => {
                checks.push(Check { name, verdict: Verdict::Fail, detail: e.to_string() });
                continue;
            }
        };
        let sim = rpc.simulate(&compiled.tx_base64, &[]).await?;
        checks.push(match &sim.err {
            None => Check {
                name,
                verdict: Verdict::Pass,
                detail: format!(
                    "simulated cleanly against live state ({} compute units)",
                    sim.units_consumed.unwrap_or(0)
                ),
            },
            Some(e) => {
                let (verdict, detail) = classify(e, &sim.logs);
                if verdict != Verdict::Pass && std::env::args().any(|a| a == "--raw") {
                    println!("        raw error: {e}");
                    for l in sim.logs.iter().rev().take(8).rev() {
                        println!("        log: {l}");
                    }
                }
                Check { name, verdict, detail }
            }
        });
    }

    Ok(PoolReport { address: raw.address.clone(), label: raw.label.clone(), dex, checks })
}

/// Raydium AMM v4.
///
/// # What is actually being tested here
///
/// The encoder passes the pool's own address for the nine OpenBook accounts, on the
/// evidence that landed transactions do the same. Evidence from four pools is not a
/// proof about the fifth, and reading a program's source is not the same as watching
/// it accept an account list. A simulation against live state is the only thing that
/// settles it, which is what this does — per pool, so the answer is per pool.
///
/// A pass here means the program ran the swap to completion with filler in those
/// slots. A failure naming a market account means that pool still wants the real ones
/// and must stay unencoded; a failure naming the *simulating address's* token accounts
/// means only that the address does not hold the mint, which [`classify`] separates.
async fn check_v4(
    rpc: &Rpc,
    simulate_as: Option<Pubkey>,
    raw: &RawPool,
    pool: Pubkey,
    pool_data: &[u8],
    mut checks: Vec<Check>,
) -> Result<PoolReport> {
    let dex = Dex::RaydiumAmmV4;
    let report = |checks| {
        Ok(PoolReport { address: raw.address.clone(), label: raw.label.clone(), dex, checks })
    };

    let amm = match cb_dex::raydium_v4::decode_amm_info(pool_data) {
        Ok(a) => a,
        Err(e) => {
            checks.push(Check { name: "pool", verdict: Verdict::Fail, detail: e.to_string() });
            return report(checks);
        }
    };
    checks.push(Check {
        name: "pool",
        verdict: Verdict::Pass,
        detail: format!("AmmInfo decodes, swap fee {} ppm", amm.fee_ppm()),
    });

    // The vaults the encoder names come from the pool account, so if the offsets were
    // wrong they would be wrong here in a way no simulation error would explain. Check
    // they really are token accounts of the pool's own two mints before going further.
    let vaults =
        rpc.accounts_full(&[to_pubkey(&amm.base_vault), to_pubkey(&amm.quote_vault)]).await?;
    for (name, slot, want) in
        [("coin vault", vaults.first(), amm.base_mint), ("pc vault", vaults.get(1), amm.quote_mint)]
    {
        let check = match slot.and_then(Option::as_ref) {
            None => Check {
                name,
                verdict: Verdict::Fail,
                detail: "the vault named by AmmInfo does not exist".into(),
            },
            Some(acc) => match token_account_mint(&acc.data) {
                Err(e) => Check { name, verdict: Verdict::Fail, detail: e.to_string() },
                Ok(mint) if mint == want => Check {
                    name,
                    verdict: Verdict::Pass,
                    detail: "holds the mint AmmInfo says it does".into(),
                },
                Ok(_) => Check {
                    name,
                    verdict: Verdict::Fail,
                    detail: "vault holds a different mint than AmmInfo claims — bad offsets".into(),
                },
            },
        };
        let failed = check.verdict == Verdict::Fail;
        checks.push(check);
        if failed {
            return report(checks);
        }
    }

    let Some(owner) = simulate_as else {
        checks.push(Check {
            name: "swap",
            verdict: Verdict::Inconclusive,
            detail: "skipped: pass --as <funded address> to check the instruction".into(),
        });
        return report(checks);
    };

    let token_program = pk(programs::SPL_TOKEN);
    let ctx = SwapContext {
        owner,
        pool,
        user_source: associated_token_address(&owner, &to_pubkey(&amm.base_mint), &token_program),
        user_dest: associated_token_address(&owner, &to_pubkey(&amm.quote_mint), &token_program),
        amount_in: PROBE_AMOUNT,
        min_amount_out: 1,
        input_is_a: true,
        input_token_program: token_program,
        output_token_program: token_program,
        // Unused by this venue. Deliberately the pool, so that if it ever were read the
        // failure names an account this file mentions rather than a random key.
        tick_arrays: [pool; cb_executor::pda::TICK_ARRAYS_PER_SWAP],
    };

    let ix = match venue::build_swap(dex, &ctx, pool_data, &VenueExtra::default()) {
        Ok(i) => i,
        Err(e) => {
            checks.push(Check { name: "swap", verdict: Verdict::Fail, detail: e.to_string() });
            return report(checks);
        }
    };

    let mut probe = vec![tx::set_compute_limit(600_000)];
    for mint in [&amm.base_mint, &amm.quote_mint] {
        let m = to_pubkey(mint);
        let ata = associated_token_address(&owner, &m, &token_program);
        probe.push(tx::create_ata_idempotent(&owner, &ata, &owner, &m, &token_program));
    }
    probe.push(ix);

    let (blockhash, _) = rpc.latest_blockhash().await?;
    let compiled = match tx::compile_unsigned(&owner, &probe, blockhash) {
        Ok(a) => a,
        Err(e) => {
            checks.push(Check { name: "swap", verdict: Verdict::Fail, detail: e.to_string() });
            return report(checks);
        }
    };
    let sim = rpc.simulate(&compiled.tx_base64, &[]).await?;
    checks.push(match &sim.err {
        None => Check {
            name: "swap",
            verdict: Verdict::Pass,
            detail: format!(
                "simulated cleanly with the pool standing in for all nine market \
                 accounts ({} compute units)",
                sim.units_consumed.unwrap_or(0)
            ),
        },
        Some(e) => {
            let (verdict, detail) = classify(e, &sim.logs);
            // Printed whatever the verdict, unlike the concentrated-liquidity path
            // above. A pass here rests on the claim that nine accounts go unread, and
            // the only thing that can show how far the program actually got before it
            // stopped is the log it wrote on the way.
            if std::env::args().any(|a| a == "--raw") {
                println!("        raw error: {e}");
                for l in sim.logs.iter().rev().take(8).rev() {
                    println!("        log: {l}");
                }
            }
            Check { name: "swap", verdict, detail }
        }
    });

    report(checks)
}

fn print_last(reports: &[PoolReport], verbose: bool) {
    let Some(r) = reports.last() else { return };
    let worst = if r.failed() {
        "FAIL"
    } else if r.all_passed() {
        "ok"
    } else {
        "?"
    };
    println!("{worst:>4}  {:<28} {}", r.label, &r.address[..8]);
    for c in &r.checks {
        if verbose || c.verdict != Verdict::Pass {
            println!("        {:<16} {:<5} {}", c.name, c.verdict.mark(), c.detail);
        }
    }
}

fn summarise(reports: &[PoolReport]) {
    use std::collections::BTreeMap;

    let total = reports.len();
    let failed = reports.iter().filter(|r| r.failed()).count();

    // Per check, because "14 pools inconclusive" hides that every vault and every
    // readable tick array passed and only the parts needing a funded address did not.
    let mut tally: BTreeMap<&str, [usize; 3]> = BTreeMap::new();
    for r in reports {
        for c in &r.checks {
            let slot = tally.entry(c.name).or_insert([0; 3]);
            slot[match c.verdict {
                Verdict::Pass => 0,
                Verdict::Fail => 1,
                Verdict::Inconclusive => 2,
            }] += 1;
        }
    }

    println!();
    println!("{total} pools checked, {failed} with a failing check");
    println!();
    println!("  {:<16} {:>6} {:>6} {:>6}", "check", "pass", "FAIL", "?");
    for (name, counts) in &tally {
        println!("  {name:<16} {:>6} {:>6} {:>6}", counts[0], counts[1], counts[2]);
    }
    println!();

    if failed > 0 {
        println!("A failure means an encoder is wrong. Do not trade against it.");
        return;
    }

    println!("No check contradicted the encoders.");
    println!();
    println!("What a clean run establishes: the account offsets, the PDA derivations, the");
    println!("discriminators, and — for every pool the swap column covers — the account");
    println!("order, all agreeing with live mainnet. A skipped swap is a pool with no tick");
    println!("arrays, which nothing can trade whatever the encoding.");
    println!();
    println!("What it still does not establish: the arithmetic of a trade. These probes");
    println!("swap a token the address does not hold, so they prove the instruction is");
    println!("well formed and stop at the balance. Whether a cycle is *profitable* is a");
    println!("different question, and the only honest answer to it is a funded dry run.");
}

/// Meteora DLMM, which shares almost nothing with the other three.
///
/// No ticks, no spacing, no oracle to derive — the oracle is named by the pool account
/// itself — and its depth lives in bin arrays whose addresses are derived from the active
/// bin. The derivation is the thing most worth checking, because a bin array seed's
/// endianness is not something any amount of reading settles: this venue seeds an `i64`
/// little-endian while Raydium seeds its tick arrays big-endian, and both produce real
/// addresses.
///
/// The probe always spends the wrapped-SOL side, wrapping it inside the probe. That is
/// not for convenience: a simulation needs a source account with a balance, and a wallet
/// holds *native* SOL rather than wSOL, so any other choice would make the check depend
/// on which tokens the address passed to `--as` happens to hold.
async fn check_dlmm(
    rpc: &Rpc,
    simulate_as: Option<Pubkey>,
    raw: &RawPool,
    pool: Pubkey,
    pool_data: &[u8],
    mut checks: Vec<Check>,
) -> Result<PoolReport> {
    let dex = Dex::MeteoraDlmm;
    let report = |checks| {
        Ok(PoolReport { address: raw.address.clone(), label: raw.label.clone(), dex, checks })
    };

    let pair = match cb_dex::meteora_dlmm::decode(pool_data) {
        Ok(p) => p,
        Err(e) => {
            checks.push(Check { name: "pool", verdict: Verdict::Fail, detail: e.to_string() });
            return report(checks);
        }
    };
    checks.push(Check {
        name: "pool",
        verdict: Verdict::Pass,
        detail: format!(
            "LbPair decodes, bin step {} ({:.2} bp base fee), active bin {} in array {}",
            pair.bin_step,
            pair.base_fee_bps(),
            pair.active_id,
            cb_dex::meteora_dlmm::bin_array_index(pair.active_id)
        ),
    });

    // The reserves the encoder names come out of the pool account, so a wrong offset
    // would be wrong here in a way no simulation error would explain.
    let reserves =
        rpc.accounts_full(&[to_pubkey(&pair.reserve_x), to_pubkey(&pair.reserve_y)]).await?;
    for (name, slot, want) in [
        ("reserve_x", reserves.first(), pair.token_x_mint),
        ("reserve_y", reserves.get(1), pair.token_y_mint),
    ] {
        let check = match slot.and_then(Option::as_ref) {
            None => Check {
                name,
                verdict: Verdict::Fail,
                detail: "the reserve named by LbPair does not exist".into(),
            },
            Some(acc) => match token_account_mint(&acc.data) {
                Err(e) => Check { name, verdict: Verdict::Fail, detail: e.to_string() },
                Ok(mint) if mint == want => Check {
                    name,
                    verdict: Verdict::Pass,
                    detail: "holds the mint LbPair says it does".into(),
                },
                Ok(_) => Check {
                    name,
                    verdict: Verdict::Fail,
                    detail: "reserve holds a different mint than LbPair claims — bad offsets"
                        .into(),
                },
            },
        };
        let failed = check.verdict == Verdict::Fail;
        checks.push(check);
        if failed {
            return report(checks);
        }
    }

    // The derivation, both ways. A swap walks one way, but the question here is whether
    // the seeds land on real arrays that name this pool, and a pool whose liquidity all
    // sits on one side of the price has nothing to find on the other.
    let mut active_bytes: Option<Vec<u8>> = None;
    // Every array either direction turns up, because that is what the live watcher holds:
    // it subscribes to a window around the active bin, not to one account. Pricing this
    // check off a single array would report a pool as unquotable that the bot quotes
    // perfectly well.
    let mut window: Vec<Vec<u8>> = Vec::new();
    for falling in [true, false] {
        let arrays = venue::meteora_dlmm::bin_arrays_for(&pool, pair.active_id, falling);
        let fetched = rpc.accounts_full(&arrays).await?;
        let mut live = 0;
        let mut foreign = 0;
        for acc in fetched.iter().flatten() {
            match cb_dex::meteora_dlmm::decode_bin_array(&acc.data) {
                Ok(a) if a.lb_pair == pool.to_bytes() => {
                    live += 1;
                    if !window.contains(&acc.data) {
                        window.push(acc.data.clone());
                    }
                    if a.index == cb_dex::meteora_dlmm::bin_array_index(pair.active_id) {
                        active_bytes = Some(acc.data.clone());
                    }
                }
                Ok(_) => foreign += 1,
                Err(_) => foreign += 1,
            }
        }
        let side = if falling { "downwards" } else { "upwards" };
        checks.push(if foreign > 0 {
            Check {
                name: "bin arrays",
                verdict: Verdict::Fail,
                detail: format!(
                    "{foreign} of the three addresses derived {side} hold something that is \
                     not this pool's bin array — the seed layout is wrong"
                ),
            }
        } else if live == 0 {
            Check {
                name: "bin arrays",
                verdict: Verdict::Inconclusive,
                detail: format!("none of the three arrays derived {side} exist yet"),
            }
        } else {
            Check {
                name: "bin arrays",
                verdict: Verdict::Pass,
                detail: format!("{live} of 3 derived {side} exist and name this pool"),
            }
        });
    }

    // And the quote has to come out of those bins, which is the check that the decoder
    // and the encoder are looking at the same pool.
    let held: Vec<&[u8]> = window.iter().map(Vec::as_slice).collect();
    let Some(_active) = active_bytes else {
        checks.push(Check {
            name: "quote",
            verdict: Verdict::Inconclusive,
            detail: "the array holding the active bin does not exist, so there is nothing to \
                     price from"
                .into(),
        });
        return report(checks);
    };
    checks.push(
        match cb_dex::meteora_dlmm::to_pool_state(pool.to_bytes(), pool_data, &held, 0) {
            Err(e) => Check { name: "quote", verdict: Verdict::Fail, detail: e.to_string() },
            Ok(state) => Check {
                name: "quote",
                verdict: Verdict::Pass,
                detail: format!(
                    "prices at {:.6} raw with {} ppm from {} arrays, depth {} / {} base units",
                    state.spot_price().unwrap_or(0.0),
                    state.fee_ppm,
                    held.len(),
                    state.reserve_a(),
                    state.reserve_b()
                ),
            },
        },
    );

    let Some(owner) = simulate_as else {
        checks.push(Check {
            name: "swap",
            verdict: Verdict::Inconclusive,
            detail: "skipped: pass --as <funded address> to check the instruction".into(),
        });
        return report(checks);
    };

    let wsol = pk(programs::WSOL_MINT);
    let (input_mint, output_mint) = if to_pubkey(&pair.token_x_mint) == wsol {
        (pair.token_x_mint, pair.token_y_mint)
    } else if to_pubkey(&pair.token_y_mint) == wsol {
        (pair.token_y_mint, pair.token_x_mint)
    } else {
        checks.push(Check {
            name: "swap",
            verdict: Verdict::Inconclusive,
            detail: "neither side is wrapped SOL, so the probe cannot fund itself".into(),
        });
        return report(checks);
    };
    let input_is_a = input_mint == pair.token_x_mint;

    let token_program = pk(programs::SPL_TOKEN);
    let source = associated_token_address(&owner, &to_pubkey(&input_mint), &token_program);
    let dest = associated_token_address(&owner, &to_pubkey(&output_mint), &token_program);
    let ctx = SwapContext {
        owner,
        pool,
        user_source: source,
        user_dest: dest,
        amount_in: PROBE_AMOUNT,
        min_amount_out: 1,
        input_is_a,
        input_token_program: token_program,
        output_token_program: token_program,
        tick_arrays: venue::meteora_dlmm::bin_arrays_for(&pool, pair.active_id, input_is_a),
    };

    let ix = match venue::build_swap(dex, &ctx, pool_data, &VenueExtra::default()) {
        Ok(i) => i,
        Err(e) => {
            checks.push(Check { name: "swap", verdict: Verdict::Fail, detail: e.to_string() });
            return report(checks);
        }
    };

    let mut probe = vec![tx::set_compute_limit(600_000)];
    for mint in [&input_mint, &output_mint] {
        let m = to_pubkey(mint);
        let ata = associated_token_address(&owner, &m, &token_program);
        probe.push(tx::create_ata_idempotent(&owner, &ata, &owner, &m, &token_program));
    }
    // Wrap the probe amount, so the source account has the balance the swap will spend.
    probe.push(tx::transfer_lamports(&owner, &source, PROBE_AMOUNT));
    probe.push(tx::sync_native(&source));
    probe.push(ix);
    probe.push(tx::close_account(&source, &owner, &owner));

    let (blockhash, _) = rpc.latest_blockhash().await?;
    let compiled = match tx::compile_unsigned(&owner, &probe, blockhash) {
        Ok(a) => a,
        Err(e) => {
            checks.push(Check { name: "swap", verdict: Verdict::Fail, detail: e.to_string() });
            return report(checks);
        }
    };
    let sim = rpc.simulate(&compiled.tx_base64, &[]).await?;
    checks.push(match &sim.err {
        None => Check {
            name: "swap",
            verdict: Verdict::Pass,
            detail: format!(
                "swap2 simulated cleanly, wrapping {PROBE_AMOUNT} lamports and naming three \
                 derived bin arrays ({} compute units)",
                sim.units_consumed.unwrap_or(0)
            ),
        },
        Some(err) => {
            let (verdict, detail) = classify(err, &sim.logs);
            Check { name: "swap", verdict, detail }
        }
    });

    report(checks)
}
