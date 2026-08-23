//! The ledger, read directly.
//!
//! The dashboard reached this data over HTTP from the bot's own server, which meant a
//! stopped bot showed no history at all. Linking `cb-ledger` removes the dependency:
//! the file is on disk whether or not anything is running.
//!
//! Read-only, always. This process must never migrate, create, or write the
//! measurement.

use std::path::Path;

/// Detections this far apart in slots belong to different episodes. Must equal
/// `cb_server::routes::EPISODE_GAP_SLOTS`, or the app and `cb-bot --report` will
/// disagree about how many opportunities there were.
pub const EPISODE_GAP_SLOTS: u64 = 5;
/// Points the curve is downsampled to. Enough to draw days without shipping a row per
/// detection into a webview.
pub const CURVE_POINTS: usize = 600;
/// Opportunities returned for the value-against-lifetime scatter, kept by value so a
/// long run still ships the points that decide the question rather than its dust.
pub const SCATTER_POINTS: usize = 1500;

/// Everything the history panels need, or an explicit statement that there is none.
///
/// Deliberately infallible. A UI that has to decide what an error means will render a
/// flat line or a zero, and HANDOVER §5.1 is a long account of what that costs.
#[must_use]
pub fn snapshot(db: &Path) -> serde_json::Value {
    if !db.exists() {
        return serde_json::json!({
            "available": false,
            "reason": format!("no ledger at {}", db.display()),
        });
    }
    match read(db) {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "available": false, "reason": e.to_string() }),
    }
}

fn read(db: &Path) -> anyhow::Result<serde_json::Value> {
    let path = db.to_string_lossy().to_string();
    let ledger = cb_ledger::Ledger::open_read_only(&path)?;
    let contest = ledger.contest_audit(EPISODE_GAP_SLOTS)?;
    let summary = ledger.summary()?;
    Ok(serde_json::json!({
        "available": true,
        "curve": ledger.equity_curve(EPISODE_GAP_SLOTS, CURVE_POINTS)?,
        "ladder": ledger.capital_ladder(EPISODE_GAP_SLOTS)?,
        "race": ledger.race_ladder(EPISODE_GAP_SLOTS)?,
        "episodes": ledger.episode_scatter(EPISODE_GAP_SLOTS, SCATTER_POINTS)?,
        "contest": contest,
        "contestSurvivalRate": contest.contested_survival_rate(),
        "uncontestedSurvivalRate": contest.uncontested_survival_rate(),
        "contestHasEvidence": contest.has_enough_evidence(),
        "hoursObserved": ledger.hours_observed()?,
        "samples": summary.samples,
        "firstAt": summary.first_at,
        "lastAt": summary.last_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason this is an app and not a web page: with no bot running there
    /// is no server, and the window must still open and say something true.
    #[test]
    fn a_missing_ledger_reports_no_history_rather_than_failing() {
        let v = snapshot(Path::new("does-not-exist.db"));
        assert_eq!(v["available"], serde_json::json!(false));
        assert!(v["reason"].is_string(), "the UI prints this verbatim");
    }

    /// If these drift apart the app and the report will quietly count different
    /// numbers of opportunities from the same file.
    #[test]
    fn the_episode_gap_matches_the_servers_so_the_two_cannot_disagree() {
        assert_eq!(EPISODE_GAP_SLOTS, 5);
    }

    /// End-to-end against whatever ledger this machine actually has. Ignored by
    /// default because it depends on a file no other machine will have; run it with
    /// `cargo test -p cb-desk -- --ignored --nocapture` to see real numbers.
    #[test]
    #[ignore = "reads the live ledger on this machine"]
    fn the_real_ledger_reads_end_to_end() {
        // From the crate directory, not the process cwd. `cargo test` runs with the
        // package root as cwd, so a bare "cryptobot.db" here resolves inside
        // crates/desk — which is how this test first reported an empty ledger while
        // `cb-bot --report` read 52,645 samples from the real one.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("crates/desk sits two levels below the workspace root")
            .to_path_buf();
        let db = root.join("cryptobot.db");
        let db = db.as_path();
        if !db.exists() {
            eprintln!("no cryptobot.db beside the crate; nothing to check");
            return;
        }
        let v = snapshot(db);
        assert_eq!(v["available"], serde_json::json!(true), "reason: {}", v["reason"]);
        let curve = v["curve"].as_array().expect("curve is an array");
        eprintln!(
            "hours={:.2}  samples={}  curve_points={}  episodes={}  realised={}",
            v["hoursObserved"].as_f64().unwrap_or(0.0),
            v["samples"],
            curve.len(),
            v["episodes"].as_array().map_or(0, Vec::len),
            v["ladder"]["realisedUsd"],
        );
        eprintln!("ladder rungs = {}", v["ladder"]["rungs"]);
        // The app's race panel reads these; if the key is missing the panel silently
        // renders its empty state and the biggest number in the instrument disappears.
        assert!(v["race"].is_object(), "the race ladder must reach the window");
        eprintln!(
            "race rungs   = {}   declined {} episodes, ${}",
            v["race"]["rungs"], v["race"]["declinedEpisodes"], v["race"]["declinedNetUsd"]
        );
        assert!(!curve.is_empty(), "a ledger with samples must produce a curve");
    }

    #[test]
    fn a_file_that_is_not_a_database_is_reported_not_panicked_on() {
        let mut p = std::env::temp_dir();
        p.push(format!("cbdesk-notadb-{}.db", std::process::id()));
        std::fs::write(&p, b"this is not sqlite").unwrap();
        let v = snapshot(&p);
        assert_eq!(v["available"], serde_json::json!(false));
    }
}
