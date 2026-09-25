//! Read-only integrity check of a ledger: `cargo run -p cb-ledger --example check_ledger -- <path>`.
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "cryptobot.db".into());
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let check: String = conn.query_row("PRAGMA quick_check(5)", [], |r| r.get(0))?;
    println!("quick_check: {check}");
    for table in ["opportunities", "sweeps", "paper_fills"] {
        let n: Result<i64, _> =
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0));
        println!("{table}: {n:?}");
    }
    Ok(())
}
