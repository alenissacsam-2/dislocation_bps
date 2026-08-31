//! Tapping the tracing output so a connected dashboard sees each line the instant it
//! is written, instead of waiting for the next poll of the log file.
//!
//! # The file is not bypassed — this runs alongside it
//!
//! `TeeWriter` still writes every byte to stdout in the same order as before, so
//! nothing about the durable log changes: a client that connects late gets the
//! complete history back from the file exactly as it always could. What this adds is
//! a second, live copy — the same line, forwarded the moment it is formatted, to
//! whoever is already connected. The desk app polls the file to catch up on connect
//! and to resync if the stream ever drops; between those moments, this is what makes
//! the Log tab move the instant something happens rather than up to a few seconds
//! later.

use std::io::Write;
use tokio::sync::mpsc::UnboundedSender;

/// A `tracing_subscriber` writer that writes through to stdout as normal and also
/// forwards a copy of each line to `tap`.
pub struct TeeWriter {
    inner: std::io::Stdout,
    tap: UnboundedSender<String>,
}

impl TeeWriter {
    #[must_use]
    fn new(tap: UnboundedSender<String>) -> Self {
        Self { inner: std::io::stdout(), tap }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        // Best-effort. `tracing_subscriber`'s own formatter only ever produces UTF-8,
        // so the decode failing is not expected; a full receiver or one that has
        // disconnected is expected, since nobody may be watching. Either way, the
        // write the caller actually asked for must not fail because of this — logging
        // must never depend on anyone watching.
        if let Ok(s) = std::str::from_utf8(buf) {
            for line in s.lines().filter(|l| !l.is_empty()) {
                let _ = self.tap.send(line.to_string());
            }
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The `MakeWriter` `tracing_subscriber` asks for a fresh writer from, per event.
#[derive(Clone)]
pub struct TeeMakeWriter {
    tap: UnboundedSender<String>,
}

impl TeeMakeWriter {
    #[must_use]
    pub fn new(tap: UnboundedSender<String>) -> Self {
        Self { tap }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter::new(self.tap.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole tap depends on: whatever bytes are written still reach
    /// the real output, unchanged, in order. If this broke, `cb-bot.log` itself would
    /// start losing lines rather than merely the live preview being late — a far worse
    /// failure than the one this module exists to fix.
    #[test]
    fn every_byte_written_still_reaches_the_real_writer_unchanged() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut w = TeeWriter::new(tx);
        let n = w.write(b"hello\n").unwrap();
        assert_eq!(n, 6, "the byte count returned to the caller must be exact");
    }

    /// The actual point: a line written through the tap arrives on the channel, and
    /// arrives without needing anyone to poll or ask for it.
    #[tokio::test]
    async fn a_written_line_arrives_on_the_tap() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut w = TeeWriter::new(tx);
        w.write_all(b"INFO cb_bot: refused: net negative after tip\n").unwrap();
        let got = rx.recv().await.expect("the line must arrive");
        assert_eq!(got, "INFO cb_bot: refused: net negative after tip");
    }

    /// One `write` call can carry more than one formatted line — a formatter is free
    /// to batch — and every one of them must reach the tap, not just the first.
    #[tokio::test]
    async fn several_lines_in_one_write_all_arrive() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut w = TeeWriter::new(tx);
        w.write_all(b"line one\nline two\n").unwrap();
        assert_eq!(rx.recv().await.unwrap(), "line one");
        assert_eq!(rx.recv().await.unwrap(), "line two");
    }

    /// Nobody listening must not be an error the caller sees — logging must not start
    /// failing because the live stream side of it has nothing attached.
    #[test]
    fn a_dropped_receiver_does_not_fail_the_write() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let mut w = TeeWriter::new(tx);
        assert!(w.write(b"still fine\n").is_ok());
    }
}
