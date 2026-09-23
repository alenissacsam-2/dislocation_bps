//! Parse a config file and print the fields that decide what a run does, without the
//! RPC URLs (they carry API keys). `cargo run -p cb-core --example check_config`.
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "config.toml".into());
    let c = cb_core::config::Config::load(&path)?;
    println!(
        "ok: mode={:?} dry_run={} submit={:?} slots={} pins={}",
        c.mode, c.dry_run, c.submit_via, c.token_account_slots, c.extra_token_mints.len()
    );
    Ok(())
}
