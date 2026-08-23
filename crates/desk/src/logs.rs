//! The last lines of the bot's log, so that when a start fails the window can say why
//! without anyone opening a shell.

use std::path::Path;

/// The last `lines` lines of `path`, oldest first. A missing or unreadable file is an
/// empty result rather than an error: the log not existing yet is the normal state
/// before the first run, not a fault the UI should have to interpret.
#[must_use]
pub fn tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].iter().map(|s| (*s).to_string()).collect()
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
}
