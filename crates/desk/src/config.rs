//! Reading and rewriting the four numbers the operator is allowed to change.
//!
//! # Why `toml_edit` and not serde
//!
//! `config.toml` is mostly prose. Every number in it carries a paragraph explaining
//! why it is that number — where the min-trade floor comes from, what the capital
//! ladder measured, why `max_hops` stops at 3. A serde deserialise-then-serialise
//! round trip produces a valid file with all of that deleted, and nothing about the
//! result looks wrong. `toml_edit` mutates the value in place and leaves the document
//! alone.
//!
//! # What is deliberately absent
//!
//! `mode`. No code path in this application writes it. HANDOVER invariant #1 is
//! enforced by there being no mechanism, rather than by a dialog someone can click
//! through.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub capital_usd: f64,
    pub fee_buffer_usd: f64,
    pub min_trade_usd: f64,
    pub max_hops: usize,
}

/// # Errors
/// If the file cannot be read or is not valid TOML.
pub fn read_params(path: &Path) -> anyhow::Result<Params> {
    let text = std::fs::read_to_string(path)?;
    let doc: toml_edit::DocumentMut = text.parse()?;
    // A value written as `100` parses as an integer, `100.0` as a float. Accept both,
    // or editing a hand-written config fails on the round number someone typed.
    let num = |k: &str, d: f64| -> f64 {
        doc.get(k)
            .and_then(|i| i.as_float().or_else(|| i.as_integer().map(|v| v as f64)))
            .unwrap_or(d)
    };
    let int = |k: &str, d: i64| doc.get(k).and_then(toml_edit::Item::as_integer).unwrap_or(d);
    Ok(Params {
        capital_usd: num("capital_usd", 100.0),
        fee_buffer_usd: num("fee_buffer_usd", 0.20),
        min_trade_usd: num("min_trade_usd", 10.0),
        max_hops: usize::try_from(int("max_hops", 3)).unwrap_or(3),
    })
}

/// Returns the reason it is unacceptable, phrased so the UI can print it verbatim.
///
/// # Errors
/// If any parameter is outside the range the instrument can honestly measure with.
pub fn validate(p: &Params) -> Result<(), String> {
    if !p.capital_usd.is_finite() || p.capital_usd <= 0.0 {
        return Err("Capital must be a positive number.".into());
    }
    if !p.fee_buffer_usd.is_finite() || p.fee_buffer_usd < 0.0 {
        return Err("Fee buffer cannot be negative.".into());
    }
    if p.fee_buffer_usd >= p.capital_usd {
        return Err("Fee buffer must leave something to trade.".into());
    }
    if !p.min_trade_usd.is_finite() || p.min_trade_usd <= 0.0 {
        return Err("Minimum trade must be a positive number.".into());
    }
    if p.max_hops < 2 {
        return Err("A cycle needs at least 2 hops to return to where it started.".into());
    }
    if p.max_hops > 4 {
        return Err("Above 4 hops the search explodes for very little added reach.".into());
    }
    Ok(())
}

/// # Errors
/// If validation fails, or the file cannot be read, parsed, or written. On a
/// validation failure the file is left untouched.
pub fn write_params(path: &Path, p: &Params) -> anyhow::Result<()> {
    if let Err(why) = validate(p) {
        anyhow::bail!(why);
    }
    let text = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    doc["capital_usd"] = toml_edit::value(p.capital_usd);
    doc["fee_buffer_usd"] = toml_edit::value(p.fee_buffer_usd);
    doc["min_trade_usd"] = toml_edit::value(p.min_trade_usd);
    doc["max_hops"] = toml_edit::value(i64::try_from(p.max_hops).unwrap_or(3));
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
mode = "paper"
feed = "live"
rpc_ws_url = "wss://x"
# Total working capital. Caps every trade size.
capital_usd = 100.0
fee_buffer_usd = 0.20
min_trade_usd = 10.0
max_hops = 3
min_profit_lamports = 0
max_position_lamports = 20000000
"#;

    fn tmp(tag: &str, contents: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cbdesk-cfg-{}-{tag}.toml", std::process::id()));
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn reads_the_four_tunables_from_a_real_config() {
        let p = tmp("read", SAMPLE);
        let got = read_params(&p).unwrap();
        assert_eq!(got.capital_usd, 100.0);
        assert_eq!(got.min_trade_usd, 10.0);
        assert_eq!(got.max_hops, 3);
    }

    #[test]
    fn a_round_number_written_without_a_decimal_point_still_reads() {
        let p = tmp("int", "capital_usd = 250\nmax_hops = 3\n");
        assert_eq!(read_params(&p).unwrap().capital_usd, 250.0);
    }

    /// config.toml is more comment than value, and those comments are the reasoning
    /// behind every number in it. A serde round-trip would silently delete all of it.
    #[test]
    fn rewriting_a_value_keeps_every_comment_in_the_file() {
        let p = tmp("comments", SAMPLE);
        let mut params = read_params(&p).unwrap();
        params.capital_usd = 1000.0;
        write_params(&p, &params).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("# Total working capital. Caps every trade size."));
        assert!(after.contains("1000"));
    }

    #[test]
    fn rewriting_a_value_never_touches_the_mode_switch() {
        let p = tmp("mode", SAMPLE);
        let mut params = read_params(&p).unwrap();
        params.capital_usd = 500.0;
        write_params(&p, &params).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains(r#"mode = "paper""#), "mode must survive untouched");
    }

    #[test]
    fn a_negative_book_is_refused_rather_than_written() {
        let bad =
            Params { capital_usd: -1.0, fee_buffer_usd: 0.2, min_trade_usd: 10.0, max_hops: 3 };
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn zero_hops_is_refused_because_a_cycle_needs_at_least_two() {
        let bad =
            Params { capital_usd: 100.0, fee_buffer_usd: 0.2, min_trade_usd: 10.0, max_hops: 0 };
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn a_buffer_larger_than_the_book_is_refused() {
        let bad =
            Params { capital_usd: 1.0, fee_buffer_usd: 5.0, min_trade_usd: 10.0, max_hops: 3 };
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn a_rejected_edit_leaves_the_file_byte_identical() {
        let p = tmp("reject", SAMPLE);
        let before = std::fs::read_to_string(&p).unwrap();
        let bad =
            Params { capital_usd: -1.0, fee_buffer_usd: 0.2, min_trade_usd: 10.0, max_hops: 3 };
        let _ = write_params(&p, &bad);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
    }
}
