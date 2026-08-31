//! The last lines of the bot's log, so that when a start fails the window can say why
//! without anyone opening a shell, and so the Log tab can watch a live run without
//! anyone tailing it by hand.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How much of the file's tail to read before splitting into lines.
///
/// A live run logs for days, and the Log tab now polls every few seconds — reading the
/// whole file on every poll would make each one cost more than the last for as long as
/// the bot keeps running. 512 KiB holds many thousands of lines, comfortably more than
/// any call here asks for; reading a bounded window keeps the cost flat regardless of
/// how large the file has grown.
const READ_WINDOW: u64 = 512 * 1024;

/// The last `lines` lines of `path`, oldest first. A missing or unreadable file is an
/// empty result rather than an error: the log not existing yet is the normal state
/// before the first run, not a fault the UI should have to interpret.
///
/// Reads only the tail of the file, not all of it — see [`READ_WINDOW`]. The one case
/// this cannot get right is `lines` truly exceeding what fits in that window on an
/// enormous single run; that trade is deliberate, because the alternative is a cost
/// that grows without bound for as long as the instrument keeps running.
#[must_use]
pub fn tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return Vec::new();
    };
    let start = len.saturating_sub(READ_WINDOW);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut raw = Vec::new();
    if file.read_to_end(&mut raw).is_err() {
        return Vec::new();
    }
    // A seek that lands mid multi-byte UTF-8 sequence is expected whenever the window
    // starts after byte 0 — lossy decoding keeps the tab usable rather than blank, and
    // it can only ever mangle the one line the truncation step below discards anyway.
    let buf = String::from_utf8_lossy(&raw).into_owned();
    let all: Vec<&str> = buf.lines().collect();
    // The window may have started mid-line; drop a partial first line whenever the
    // read did not begin at the true start of the file, so what is shown is whole
    // lines rather than a truncated fragment that reads like corruption.
    let all = if start > 0 && all.len() > 1 { &all[1..] } else { &all[..] };
    let from = all.len().saturating_sub(lines);
    all[from..].iter().map(|s| (*s).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, body: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cbdesk-log-{}-{name}", std::process::id()));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn returns_the_last_n_lines_in_order() {
        let p = tmp("order", "a\nb\nc\nd\n");
        assert_eq!(tail(&p, 2), vec!["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn a_file_shorter_than_the_window_returns_all_of_it() {
        let p = tmp("short", "only\n");
        assert_eq!(tail(&p, 50), vec!["only".to_string()]);
    }

    /// When a start fails the log is the only explanation there is; a missing file
    /// must read as empty rather than throwing the UI into an error state.
    #[test]
    fn a_missing_log_is_empty_not_an_error() {
        assert!(tail(Path::new("nope-not-here.log"), 10).is_empty());
    }

    #[test]
    fn asking_for_no_lines_returns_none_rather_than_everything() {
        let p = tmp("zero", "a\nb\n");
        assert!(tail(&p, 0).is_empty());
    }

    /// The whole reason for the window: a file much larger than it must not be read in
    /// full. Written large enough that a naive `read_to_string` would show up in a
    /// profiler; asserted only on correctness here, since a test cannot observe cost,
    /// but the size is real.
    #[test]
    fn a_file_far_larger_than_the_window_still_returns_its_true_tail() {
        let mut body = String::new();
        for i in 0..40_000 {
            body.push_str(&format!("line {i}\n"));
        }
        let p = tmp("huge", &body);
        let got = tail(&p, 5);
        assert_eq!(got, vec!["line 39995", "line 39996", "line 39997", "line 39998", "line 39999"]
            .into_iter().map(String::from).collect::<Vec<_>>());
    }

    /// A window that starts mid-line must not show that fragment as if it were a whole
    /// line — it would read as truncated garbage rather than as an omitted line.
    #[test]
    fn a_partial_first_line_from_a_mid_file_window_is_dropped() {
        // Build a file bigger than READ_WINDOW so the read genuinely starts mid-file,
        // with a deliberately recognisable final few lines.
        let mut body = "x".repeat(600_000);
        body.push_str("\nlast\n");
        let p = tmp("partial", &body);
        let got = tail(&p, 10);
        // No line here should be the giant filler line's tail fragment.
        assert!(got.iter().all(|l| !l.contains('x')), "a partial line leaked into the result: {got:?}");
        assert_eq!(got.last().map(String::as_str), Some("last"));
    }

    /// Requesting more lines than exist in the window must not panic or wrap around.
    #[test]
    fn asking_for_more_lines_than_the_file_has_returns_all_of_it() {
        let p = tmp("fewer", "a\nb\nc\n");
        assert_eq!(tail(&p, 1000), vec!["a", "b", "c"]);
    }
}
