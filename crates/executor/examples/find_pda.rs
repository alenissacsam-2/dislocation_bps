//! Find which seed pattern derives a known address.
//!
//! `cargo run -p cb-executor --example find_pda -- <target> <program>... -- <pubkey>...`
//!
//! Tries each program with a set of common seed words, alone and followed by each given
//! pubkey (or two of them). For reverse-engineering a PDA a program started requiring
//! after its published interface was written.

use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sep = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let target = Pubkey::from_str(&args[0])?;
    let programs: Vec<Pubkey> = args[1..sep].iter().map(|s| Pubkey::from_str(s)).collect::<Result<_, _>>()?;
    let keys: Vec<Pubkey> = args.get(sep + 1..).unwrap_or(&[]).iter().map(|s| Pubkey::from_str(s)).collect::<Result<_, _>>()?;
    let words = [
        "pool_v2", "pool-v2", "poolv2", "pool_v2_state", "pool-v2-state", "pool", "pool_extension",
        "pool-extension", "pool_state", "pool-state", "amm_v2", "v2", "pool_config", "sharing-config",
        "sharing_config", "bonding-curve-v2", "bonding_curve_v2", "pool-authority", "cashback",
        "user_volume_accumulator", "creator_vault", "global_volume_accumulator", "pool_metadata",
        "pool-metadata", "fee_sharing", "buyback",
    ];
    for program in &programs {
        for w in words {
            let mut tries: Vec<Vec<Vec<u8>>> = vec![vec![w.as_bytes().to_vec()]];
            for k in &keys {
                tries.push(vec![w.as_bytes().to_vec(), k.to_bytes().to_vec()]);
                for k2 in &keys {
                    tries.push(vec![w.as_bytes().to_vec(), k.to_bytes().to_vec(), k2.to_bytes().to_vec()]);
                }
            }
            for seeds in tries {
                let refs: Vec<&[u8]> = seeds.iter().map(Vec::as_slice).collect();
                if Pubkey::find_program_address(&refs, program).0 == target {
                    println!("FOUND under {program}: word {w:?} with {} key(s)", seeds.len() - 1);
                    return Ok(());
                }
            }
        }
    }
    println!("not found");
    Ok(())
}
