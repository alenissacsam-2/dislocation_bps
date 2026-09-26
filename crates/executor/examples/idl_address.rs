//! Print the Anchor IDL account address for each program given.
//!
//! `cargo run -p cb-executor --example idl_address -- <program id>...`
//!
//! Anchor keeps a program's interface on chain at
//! `create_with_seed(find_program_address([], program), "anchor:idl", program)`.
//! Reading it from there, rather than from a copy in someone's repository, gives the
//! interface the deployed program actually has.

use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

fn main() -> anyhow::Result<()> {
    for id in std::env::args().skip(1) {
        let program = Pubkey::from_str(&id)?;
        let (base, _) = Pubkey::find_program_address(&[], &program);
        let idl = Pubkey::create_with_seed(&base, "anchor:idl", &program)?;
        println!("{id} {idl}");
    }
    Ok(())
}
