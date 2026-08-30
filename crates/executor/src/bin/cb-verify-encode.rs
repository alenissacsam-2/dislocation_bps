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
    let limit: usize =
        flag("--limit").and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);

    let raw = std::fs::read_to_string(&pools_path)
        .with_context(|| format!("could not read the pool registry at {pools_path}"))?;
    let registry: Registry = serde_json::from_str(&raw).context("pools.json is not the registry")?;

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
        return Ok(PoolReport { address: raw.address.clone(), label: raw.label.clone(), dex, checks });
    };

    let (mint_a, mint_b, vault_a, vault_b, tick, spacing, liquidity, program) =
        match dex {
            Dex::OrcaWhirlpool => {
                let w = cb_dex::orca_whirlpool::decode(&pool_data)?;
                (
                    w.mint_a, w.mint_b, w.vault_a, w.vault_b, w.tick_current, w.tick_spacing,
                    w.liquidity, pk(cb_dex::orca_whirlpool::PROGRAM_ID),
                )
            }
            _ => {
                let p = cb_dex::raydium_clmm::decode(&pool_data)?;
                (
                    p.mint_0, p.mint_1, p.vault_0, p.vault_1, p.tick_current, p.tick_spacing,
                    p.liquidity, pk(cb_dex::raydium_clmm::PROGRAM_ID),
                )
            }
        };

    // The direction the probe swap will take: spending token A pushes the price down.
    let falling = true;

    let oracle = orca_oracle(&pool, &program);
    let fetched = rpc
        .accounts_full(&[to_pubkey(&vault_a), to_pubkey(&vault_b), oracle])
        .await?;

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
                Err(e) => Check {
                    name: "vault",
                    verdict: Verdict::Fail,
                    detail: format!("{side}: {e}"),
                },
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
                "the pool has no tick arrays at any of the {} addresses swept in both                  directions, so nothing can swap through it whatever the encoding                  (tick {tick}, spacing {spacing}, liquidity {liquidity})",
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
                detail: "the derived oracle does not exist, which is normal for a pool without adaptive fees - the program treats it as a placeholder"
                    .into(),
            },
        });
    }

    // ---- 4. Simulate the instruction itself. ----
    //
    // This is the only check that needs an address which actually exists and holds the
    // pool's mints. Without one the runtime rejects the transaction before running it,
    // which says nothing about the encoder, so the check is skipped rather than failed.
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
        tick_arrays: chosen.arrays,
    };

    let policies: &[Option<BitmapPolicy>] = if dex == Dex::RaydiumClmm {
        &[Some(BitmapPolicy::Include), Some(BitmapPolicy::Omit)]
    } else {
        &[None]
    };

    for policy in policies {
        let extra = VenueExtra {
            token_program,
            bitmap_policy: policy.unwrap_or(BitmapPolicy::Include),
        };
        let name = match policy {
            Some(BitmapPolicy::Include) => "swap(bitmap)",
            Some(BitmapPolicy::Omit) => "swap(no bitmap)",
            None => "swap",
        };
        let ix = match venue::build_swap(dex, &ctx, &pool_data, &extra) {
            Ok(i) => i,
            Err(e) => {
                checks.push(Check { name, verdict: Verdict::Fail, detail: e.to_string() });
                continue;
            }
        };
        let (blockhash, _) = rpc.latest_blockhash().await?;
        // Unsigned: a placeholder signature, because simulation does not check one and
        // verification must never need a key.
        let compiled = match tx::compile_unsigned(
            &owner,
            &[tx::set_compute_limit(400_000), ix],
            blockhash,
        ) {
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
    println!("What a clean vault and tick_array column establishes: the account offsets and");
    println!("the PDA derivations agree with live mainnet. What it does not establish: the");
    println!("account *order* inside the instruction, or the arithmetic of a trade. Only the");
    println!("swap column speaks to the first, and only a funded simulation of a real cycle");
    println!("speaks to the second.");
}
