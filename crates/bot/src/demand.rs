//! Which intermediate mints are worth a token account, measured rather than named.
//!
//! # Why this exists
//!
//! A cycle through a mint the wallet holds no account for cannot pass its profit check
//! (see `Trader::accounts_held`), so which accounts exist decides which cycles are
//! reachable at all. That used to be a hand-written list in `extra_token_mints`, and it
//! went stale in exactly the way a guess does: on 2026-09-23 the top of the board for
//! four hours was a DKNG loop the wallet could not hold, while five accounts opened for
//! other mints sat idle.
//!
//! An account costs a refundable deposit, not a fee — about 0.002 SOL that comes back
//! in full when the empty account is closed — plus a base fee to open and another to
//! close. So the right policy is not "be sparing" but "keep the slots on the mints that
//! are actually asked for": open one as soon as a mint shows it is wanted, and hand back
//! the deposit of any account nothing has asked for in a day.
//!
//! # What counts as asked for
//!
//! Every cycle that reaches an execution attempt — past the edge, tip and cooldown
//! gates — records each of its intermediate mints, with the net the detector expected.
//! One record per attempt, and attempts on the same cycle are already throttled by the
//! refusal cooldown, so a single loop stuck on the board does not count a thousand
//! times. The window is rolling and persisted, so a restart does not forget it.

use cb_core::types::Pubkey32;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

/// How far back demand is remembered: a day, so a mint that only trades in one
/// market session is not evicted overnight on a technicality.
pub const WINDOW_SECS: u64 = 24 * 60 * 60;

/// How often a mint has to be asked for before an account is opened for it.
///
/// Two, not one: a single detection is often a lagged price that vanishes on re-price,
/// and one open plus one close is two base fees for nothing. Two separate attempts —
/// already at least a cooldown apart — is a mint that keeps coming back.
pub const MIN_ASKS_TO_OPEN: u32 = 2;

/// How much more a new mint must be worth than the weakest held one to take its slot.
///
/// Without a margin two mints of similar value would swap back and forth, paying two
/// base fees each time for no change in reach.
pub const REPLACE_MARGIN: f64 = 1.5;

/// The least a mint's asks must have expected between them, in USD, before an account
/// is opened for it: about the two base fees that opening and later closing cost.
///
/// Measured the first morning: the rotation opened an account for a mint asked for
/// twice at $0.0000 expected, because every ask had cleared the base fee by a hair.
pub const MIN_USD_TO_OPEN: f64 = 0.001;

/// How much one mint has been asked for over the window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Score {
    pub asks: u32,
    /// Sum of the net each asking attempt expected, in USD. Detection-time, so an
    /// overstatement, but the same overstatement for every mint: fine for ranking.
    pub usd: f64,
}

/// A rolling record of which mints execution attempts needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Demand {
    /// When recording began, in unix seconds. A mint cannot be judged idle for a day
    /// before a day has been watched.
    since: u64,
    events: VecDeque<(u64, String, f64)>,
}

impl Demand {
    #[must_use]
    pub fn new(now: u64) -> Self {
        Self { since: now, events: VecDeque::new() }
    }

    /// Load the persisted window, or start a fresh one if there is none or it cannot be
    /// read. A lost window costs a day of patience, never a wrong close: closing needs
    /// a full day watched.
    #[must_use]
    pub fn load(path: &Path, now: u64) -> Self {
        let mut d = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .filter(|d| d.since <= now)
            .unwrap_or_else(|| Self::new(now));
        d.prune(now);
        d
    }

    /// Write the window to `path`, through a temporary file so a crash mid-write
    /// leaves the previous copy rather than half of one.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// Note that an attempt needed `mint`, expecting `usd` net.
    pub fn record(&mut self, now: u64, mint: &Pubkey32, usd: f64) {
        let usd = if usd.is_finite() { usd.max(0.0) } else { 0.0 };
        self.events.push_back((now, bs58::encode(mint).into_string(), usd));
        self.prune(now);
    }

    fn prune(&mut self, now: u64) {
        let cutoff = now.saturating_sub(WINDOW_SECS);
        while self.events.front().is_some_and(|(at, _, _)| *at < cutoff) {
            self.events.pop_front();
        }
    }

    /// Whether a whole window has been watched, which is what judging a mint idle needs.
    #[must_use]
    pub fn covers_window(&self, now: u64) -> bool {
        now.saturating_sub(self.since) >= WINDOW_SECS
    }

    /// Every mint asked for in the window, with its score.
    #[must_use]
    pub fn scores(&self) -> HashMap<Pubkey32, Score> {
        let mut out: HashMap<Pubkey32, Score> = HashMap::new();
        for (_, mint, usd) in &self.events {
            let Some(k) = decode(mint) else { continue };
            let s = out.entry(k).or_default();
            s.asks += 1;
            s.usd += usd;
        }
        out
    }

    /// Mints by value over the window, best first. Ties go to the more often asked.
    #[must_use]
    pub fn ranking(&self) -> Vec<(Pubkey32, Score)> {
        let mut v: Vec<_> = self.scores().into_iter().collect();
        v.sort_by(|a, b| {
            b.1.usd.total_cmp(&a.1.usd).then(b.1.asks.cmp(&a.1.asks)).then(a.0.cmp(&b.0))
        });
        v
    }
}

fn decode(s: &str) -> Option<Pubkey32> {
    bs58::decode(s).into_vec().ok()?.try_into().ok()
}

/// What to do about a mint an attempt was just refused for want of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move {
    /// A slot is free: open it.
    Open(Pubkey32),
    /// Every slot is taken: close `close` (worth less) and open `open` in its place.
    Replace { open: Pubkey32, close: Pubkey32 },
}

/// Decide whether `wanted` earns an account now.
///
/// `managed` is the accounts this policy may close: held, and neither a base mint nor
/// one the operator pinned in `extra_token_mints`. `slots` is how many of those there
/// may be at once, which is what bounds the deposit.
#[must_use]
pub fn on_missing(
    demand: &Demand,
    managed: &HashSet<Pubkey32>,
    slots: usize,
    wanted: &Pubkey32,
) -> Option<Move> {
    if managed.contains(wanted) {
        return None;
    }
    let scores = demand.scores();
    let score_of = |m: &Pubkey32| scores.get(m).copied().unwrap_or_default();
    let want = score_of(wanted);
    if want.asks < MIN_ASKS_TO_OPEN || want.usd < MIN_USD_TO_OPEN {
        return None;
    }
    if managed.len() < slots {
        return Some(Move::Open(*wanted));
    }
    let weakest = managed.iter().min_by(|a, b| {
        let (sa, sb) = (score_of(a), score_of(b));
        sa.usd.total_cmp(&sb.usd).then(sa.asks.cmp(&sb.asks)).then(a.cmp(b))
    })?;
    let floor = score_of(weakest);
    (want.usd > floor.usd * REPLACE_MARGIN && want.asks > floor.asks)
        .then_some(Move::Replace { open: *wanted, close: *weakest })
}

/// Accounts nothing has asked for in a whole watched day. Their deposit is capital the
/// wallet can trade with instead.
#[must_use]
pub fn idle(demand: &Demand, managed: &HashSet<Pubkey32>, now: u64) -> Vec<Pubkey32> {
    if !demand.covers_window(now) {
        return Vec::new();
    }
    let scores = demand.scores();
    let mut v: Vec<_> = managed.iter().filter(|m| !scores.contains_key(*m)).copied().collect();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(b: u8) -> Pubkey32 {
        [b; 32]
    }

    fn set(ms: &[u8]) -> HashSet<Pubkey32> {
        ms.iter().map(|b| m(*b)).collect()
    }

    #[test]
    fn a_mint_asked_for_once_does_not_get_an_account() {
        let mut d = Demand::new(0);
        d.record(10, &m(1), 0.01);
        assert_eq!(on_missing(&d, &set(&[]), 4, &m(1)), None);
        d.record(20, &m(1), 0.01);
        assert_eq!(on_missing(&d, &set(&[]), 4, &m(1)), Some(Move::Open(m(1))));
    }

    #[test]
    fn a_mint_asked_for_often_but_worth_nothing_does_not_get_an_account() {
        let mut d = Demand::new(0);
        for t in 0..9 {
            d.record(t, &m(4), 0.000_01);
        }
        assert_eq!(on_missing(&d, &set(&[]), 4, &m(4)), None);
    }

    #[test]
    fn a_full_book_replaces_its_weakest_only_by_a_clear_margin() {
        let mut d = Demand::new(0);
        for t in 0..3 {
            d.record(t, &m(2), 0.010);
            d.record(t, &m(3), 0.002);
        }
        d.record(5, &m(9), 0.004);
        d.record(6, &m(9), 0.004);
        // 0.008 over 2 asks against the weakest's 0.006 over 3: not enough.
        assert_eq!(on_missing(&d, &set(&[2, 3]), 2, &m(9)), None);
        d.record(7, &m(9), 0.004);
        d.record(8, &m(9), 0.004);
        assert_eq!(
            on_missing(&d, &set(&[2, 3]), 2, &m(9)),
            Some(Move::Replace { open: m(9), close: m(3) })
        );
    }

    #[test]
    fn a_held_mint_nobody_asked_for_is_idle_only_after_a_full_day() {
        let mut d = Demand::new(0);
        d.record(100, &m(2), 0.01);
        assert!(idle(&d, &set(&[2, 3]), 1_000).is_empty(), "a day has not been watched");
        let later = WINDOW_SECS + 50;
        assert_eq!(idle(&d, &set(&[2, 3]), later), vec![m(3)]);
        // And once the window rolls past the only ask, that mint is idle too.
        let mut d2 = d.clone();
        d2.prune(WINDOW_SECS + 200);
        assert_eq!(idle(&d2, &set(&[2, 3]), WINDOW_SECS + 200), vec![m(2), m(3)]);
    }

    #[test]
    fn the_ranking_orders_by_value_then_asks() {
        let mut d = Demand::new(0);
        d.record(1, &m(1), 0.001);
        d.record(1, &m(2), 0.005);
        d.record(2, &m(3), 0.001);
        d.record(3, &m(3), 0.000);
        let r: Vec<_> = d.ranking().into_iter().map(|(k, _)| k).collect();
        assert_eq!(r, vec![m(2), m(3), m(1)]);
    }

    #[test]
    fn the_window_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("cb-demand-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token-demand.json");
        let mut d = Demand::new(0);
        d.record(10, &m(7), 0.02);
        d.save(&path).unwrap();
        let back = Demand::load(&path, 20);
        assert_eq!(back.scores().get(&m(7)).map(|s| s.asks), Some(1));
        assert!(!back.covers_window(20));
        let _ = std::fs::remove_dir_all(dir);
    }
}
