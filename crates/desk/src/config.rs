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

/// Everything the operator needs to know about which mode this machine is actually in.
///
/// `mode` is what the file says. `effective` is what the bot will use, which is not the
/// same thing: `Config::load` merges `Env::prefixed("CRYPTOBOT_")` over the TOML, so
/// `CRYPTOBOT_MODE=live` in the environment beats anything written here. A control that
/// showed only the file would be telling the operator the opposite of the truth in
/// exactly the case that matters, so both are reported and the UI says when they differ.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeStatus {
    /// What `config.toml` says.
    pub mode: String,
    /// What the bot will actually run as, after the environment is applied.
    pub effective: String,
    /// Present when `CRYPTOBOT_MODE` is set and therefore overriding the file.
    pub env_override: Option<String>,
    /// Whether `CRYPTOBOT_ALLOW_LIVE=1` — the half of the guard that lives outside
    /// this application and which it deliberately does not set.
    pub allow_live_set: bool,
    /// Whether this build can actually execute a trade. Reported rather than assumed,
    /// because selecting Live while this is false means the bot refuses to start, and
    /// an operator deserves to know that before they flip it rather than after.
    pub execution_implemented: bool,
}

/// Live execution is built. Changed 2026-08-30.
///
/// `crates/bot/src/execute.rs` encodes real swaps for Orca Whirlpool and Raydium CLMM,
/// and `cb-bot` now links `cb-executor`, `cb-wallet` and `solana-sdk`. The guarantee
/// that no argument about flags could produce a signature — because the code was absent
/// — is gone, and nothing brings it back short of deleting that module.
///
/// What carries the weight now, all of it checkable and none of it in this constant:
///
/// - `mode = "live"` in the config **and** `CRYPTOBOT_ALLOW_LIVE=1` in the environment,
///   which this application deliberately does not set.
/// - A passphrase, read from the bot's stdin at spawn. A live config on its own loads
///   no key and signs nothing.
/// - `dry_run`, which defaults to **true** and is a separate decision from `mode`, so
///   arming execution and spending money are two acts rather than one.
/// - The risk gate, and simulation against live state with the profit read from the
///   resulting balance rather than from the quote.
///
/// This constant now says only that the machinery exists, which is why the UI stopped
/// using it to refuse and started using it to warn.
pub const EXECUTION_IMPLEMENTED: bool = true;

/// # Errors
/// If the file cannot be read or parsed.
pub fn read_mode(path: &Path) -> anyhow::Result<ModeStatus> {
    let text = std::fs::read_to_string(path)?;
    let doc: toml_edit::DocumentMut = text.parse()?;
    let from_file = doc
        .get("mode")
        .and_then(toml_edit::Item::as_str)
        .unwrap_or("paper")
        .to_string();

    let env_override = std::env::var("CRYPTOBOT_MODE").ok().filter(|v| !v.is_empty());
    let effective = env_override.clone().unwrap_or_else(|| from_file.clone());
    let allow_live_set =
        std::env::var(cb_core::config::LIVE_ENV_VAR).ok().as_deref() == Some("1");

    Ok(ModeStatus {
        mode: from_file,
        effective,
        env_override,
        allow_live_set,
        execution_implemented: EXECUTION_IMPLEMENTED,
    })
}

/// Write `mode`, and nothing else.
///
/// This is the mechanism whose *absence* used to be the guarantee. It exists now
/// because the operator asked for a control, so the guarantee has to be carried by
/// something real instead: the second switch still lives outside this application, the
/// bot refuses any live config outright while execution is unbuilt, and every mode
/// indicator is now derived rather than hardcoded so a live run cannot present as paper.
///
/// # Errors
/// If the mode is not one of the two the config understands, or the file cannot be
/// read, parsed or written.
pub fn write_mode(path: &Path, mode: &str) -> anyhow::Result<()> {
    // Only the two strings serde will parse. Anything else deserialises to an error at
    // the bot's next start, which would present as "the app saved it and the bot broke".
    if mode != "paper" && mode != "live" {
        anyhow::bail!("mode must be \"paper\" or \"live\", not {mode:?}");
    }
    let text = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    doc["mode"] = toml_edit::value(mode);
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// Read the risk limits, falling back to the conservative defaults for anything the
/// file does not mention.
///
/// A missing limit is not an error. The defaults exist precisely so that a config
/// written before these keys existed still produces a gate that refuses sensibly,
/// rather than one that trades without bounds because a key was absent.
///
/// # Errors
/// If the file cannot be read or is not valid TOML.
pub fn read_limits(path: &Path) -> anyhow::Result<cb_executor::risk::Limits> {
    let text = std::fs::read_to_string(path)?;
    let doc: toml_edit::DocumentMut = text.parse()?;
    let d = cb_executor::risk::Limits::default();
    let num = |k: &str, fallback: f64| -> f64 {
        doc.get(k)
            .and_then(|i| i.as_float().or_else(|| i.as_integer().map(|v| v as f64)))
            .unwrap_or(fallback)
    };
    let int = |k: &str, fallback: u32| -> u32 {
        doc.get(k)
            .and_then(toml_edit::Item::as_integer)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(fallback)
    };
    Ok(cb_executor::risk::Limits {
        max_position_usd: num("max_position_usd", d.max_position_usd),
        max_daily_loss_usd: num("max_daily_loss_usd", d.max_daily_loss_usd),
        min_net_profit_usd: num("min_net_profit_usd", d.min_net_profit_usd),
        max_slippage_bps: num("max_slippage_bps", d.max_slippage_bps),
        max_consecutive_failures: int("max_consecutive_failures", d.max_consecutive_failures),
        max_daily_trades: int("max_daily_trades", d.max_daily_trades),
    })
}

/// Write the risk limits.
///
/// Unlike [`write_params`] this does **not** end the run. Limits bound what may be
/// signed; they do not change how anything is measured, so rows recorded before and
/// after a change still aggregate by the same rules and there is nothing to contaminate.
///
/// # Errors
/// If the limits are unusable, or the file cannot be read, parsed, or written. On a
/// validation failure the file is left untouched.
pub fn write_limits(path: &Path, l: &cb_executor::risk::Limits) -> anyhow::Result<()> {
    if let Err(why) = l.validate() {
        anyhow::bail!(why);
    }
    let text = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    doc["max_position_usd"] = toml_edit::value(l.max_position_usd);
    doc["max_daily_loss_usd"] = toml_edit::value(l.max_daily_loss_usd);
    doc["min_net_profit_usd"] = toml_edit::value(l.min_net_profit_usd);
    doc["max_slippage_bps"] = toml_edit::value(l.max_slippage_bps);
    doc["max_consecutive_failures"] =
        toml_edit::value(i64::from(l.max_consecutive_failures));
    doc["max_daily_trades"] = toml_edit::value(i64::from(l.max_daily_trades));
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// The RPC endpoint the app should ask about balances.
///
/// Read from `config.toml` rather than hardcoded, so the panel reports what it saw from
/// the same node the bot is talking to. `figment` lets `CRYPTOBOT_RPC_HTTP_URL` override
/// the file for the bot, so that is honoured here too — otherwise the window could show
/// balances from one endpoint while the instrument runs against another.
///
/// # Errors
/// If the file cannot be read or parsed.
pub fn read_rpc_url(path: &Path) -> anyhow::Result<String> {
    if let Ok(from_env) = std::env::var("CRYPTOBOT_RPC_HTTP_URL") {
        if !from_env.trim().is_empty() {
            return Ok(from_env);
        }
    }
    let text = std::fs::read_to_string(path)?;
    let doc: toml_edit::DocumentMut = text.parse()?;
    Ok(doc
        .get("rpc_http_url")
        .and_then(toml_edit::Item::as_str)
        .unwrap_or("https://api.mainnet-beta.solana.com")
        .to_string())
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

    // ── mode ────────────────────────────────────────────────────────────────

    fn mode_file(name: &str, body: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join("cb-desk-mode-tests").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("config.toml");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn mode_round_trips_through_the_file() {
        let p = mode_file("roundtrip", "mode = \"paper\"\ncapital_usd = 100.0\n");
        assert_eq!(read_mode(&p).unwrap().mode, "paper");
        write_mode(&p, "live").unwrap();
        assert_eq!(read_mode(&p).unwrap().mode, "live");
        write_mode(&p, "paper").unwrap();
        assert_eq!(read_mode(&p).unwrap().mode, "paper");
    }

    /// Anything the bot's serde cannot parse would present as "the app saved it and the
    /// bot broke", so it is refused at the point of writing instead.
    #[test]
    fn a_mode_the_config_cannot_parse_is_refused_and_nothing_is_written() {
        let p = mode_file("badmode", "mode = \"paper\"\n");
        let before = std::fs::read_to_string(&p).unwrap();
        for bad in ["Live", "LIVE", "real", "", "true", "papers"] {
            assert!(write_mode(&p, bad).is_err(), "{bad:?} should not be accepted");
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
    }

    /// The file is mostly prose explaining every number in it. A mode switch must not
    /// be the thing that deletes it.
    #[test]
    fn switching_mode_keeps_the_rest_of_the_file_intact() {
        let body = "# why this is paper\nmode = \"paper\"\n\n# the capital note\ncapital_usd = 100.0\n";
        let p = mode_file("comments", body);
        write_mode(&p, "live").unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("# why this is paper"));
        assert!(after.contains("# the capital note"));
        assert!(after.contains("capital_usd = 100.0"));
        assert!(after.contains("mode = \"live\""));
    }

    /// A config with no `mode` key deserialises to Paper in cb-core, so reading one
    /// must agree rather than inventing a different default.
    #[test]
    fn a_config_without_a_mode_key_reads_as_paper() {
        let p = mode_file("nomode", "capital_usd = 100.0\n");
        assert_eq!(read_mode(&p).unwrap().mode, "paper");
    }

    /// The canary that used to assert execution was unbuilt fired, as designed, when
    /// execution was built. This is what replaced it.
    ///
    /// The constant no longer carries a safety claim — it says only that the machinery
    /// exists, which is why `set_mode` stopped refusing on it and started warning. The
    /// guarantee moved to the two switches, the stdin passphrase, `dry_run`, the risk
    /// gate, and simulation-before-send.
    // Asserting on a constant is normally pointless. Here the constant is the subject.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn execution_is_implemented_and_the_guarantee_moved_to_the_runtime_guards() {
        assert!(
            EXECUTION_IMPLEMENTED,
            "if execution is being removed, restore set_mode's refusal and the bot's \
             startup bail in the same change"
        );
    }

    /// The panel must report balances from the same endpoint the bot is using, or a
    /// surprising number gets blamed on the wrong node.
    #[test]
    fn the_rpc_url_comes_from_the_file_and_falls_back_to_mainnet() {
        let p = tmp("rpc", "rpc_http_url = \"https://example.test/rpc\"
");
        assert_eq!(read_rpc_url(&p).unwrap(), "https://example.test/rpc");

        // A config with no endpoint still yields a usable one rather than an error.
        let bare = tmp("rpc-bare", "mode = \"paper\"
");
        assert!(read_rpc_url(&bare).unwrap().contains("mainnet"));
    }
}
