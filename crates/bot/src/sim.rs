//! A synthetic market, used to exercise the full pipeline before a live feed exists.
//!
//! This is **not** a toy that fabricates opportunities. It generates plausible pool
//! reserves, then runs them through the *real* `cb_core::amm` maths — the same code
//! that will price live pools. If the maths says there is no arbitrage, the simulator
//! reports none. That makes it a genuine integration test of the pricing path, and it
//! means the dashboard is showing real computation over synthetic inputs rather than
//! theatre.
//!
//! Everything it emits is tagged so the UI can label it unmistakably as simulated.

use cb_core::amm::{cycle_profit, optimal_input, CycleReserves};
use cb_server::{Event, EventBus};
use std::time::{SystemTime, UNIX_EPOCH};

const SOL_USD: f64 = 76.97;
const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Total capital, in USD. The binding constraint on every trade.
const CAPITAL_USD: f64 = 5.0;
/// Held back for fees and rent; not tradable.
const FEE_BUFFER_USD: f64 = 0.20;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// Deterministic-ish pseudo-random source. Avoids a dependency and keeps runs
/// reproducible from a seed, which matters when a surprising number shows up on the
/// dashboard and we need to reproduce it.
pub struct Rng(u64);

impl Rng {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Uniform in [lo, hi).
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + self.unit() * (hi - lo)
    }
    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

struct Venue {
    dex: &'static str,
    /// Reserve of the base token (SOL side), in base units.
    reserve_a: u128,
    /// Reserve of the counter token (USDC side), in base units.
    reserve_b: u128,
    fee_bps: u32,
}

impl Venue {
    /// Price of base in counter terms, adjusting for the 9 vs 6 decimal difference
    /// between SOL and USDC.
    fn price(&self) -> f64 {
        if self.reserve_a == 0 {
            return 0.0;
        }
        (self.reserve_b as f64 / 1e6) / (self.reserve_a as f64 / 1e9)
    }
}

pub struct Market {
    rng: Rng,
    venues: Vec<Venue>,
    pair: &'static str,
    slot: u64,
    next_id: u64,
    started: std::time::Instant,
}

impl Market {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        // Two constant-product venues on SOL/USDC, seeded near the live SOL price.
        let mk = |dex, sol: f64, fee| Venue {
            dex,
            reserve_a: (sol * 1e9) as u128,
            reserve_b: (sol * SOL_USD * 1e6) as u128,
            fee_bps: fee,
        };
        Self {
            rng: Rng::new(seed),
            venues: vec![mk("Raydium v4", 4_200.0, 25), mk("PumpSwap", 1_800.0, 25)],
            pair: "SOL/USDC",
            slot: 380_000_000,
            next_id: 1,
            started: std::time::Instant::now(),
        }
    }

    /// Advance one slot: nudge reserves, emit updates, and price any real cycle.
    pub fn tick(&mut self, bus: &EventBus) {
        self.slot += 1;
        let ts = now_ms();

        // Random-walk each venue's counter reserve. Occasionally shock one venue,
        // which is what actually creates a cross-venue dislocation.
        for i in 0..self.venues.len() {
            let drift = self.rng.range(-0.0006, 0.0006);
            let shock = if self.rng.chance(0.04) {
                self.rng.range(-0.010, 0.010)
            } else {
                0.0
            };
            let v = &mut self.venues[i];
            let factor = 1.0 + drift + shock;
            v.reserve_b = ((v.reserve_b as f64) * factor).max(1.0) as u128;
        }

        for v in &self.venues {
            bus.publish(Event::PoolUpdate {
                pool: format!("sim-{}", v.dex.to_lowercase().replace([' ', '.'], "-")),
                dex: v.dex.to_string(),
                pair: self.pair.to_string(),
                price: v.price(),
                reserve_a: v.reserve_a as f64 / 1e9,
                reserve_b: v.reserve_b as f64 / 1e6,
                slot: self.slot,
                ts_ms: ts,
            });
        }

        self.price_cycle(bus, ts);
    }

    /// Run the real arbitrage maths over the current synthetic reserves.
    fn price_cycle(&mut self, bus: &EventBus, ts: u64) {
        let (a, b) = (&self.venues[0], &self.venues[1]);

        // Direction: buy where SOL is cheap, sell where it is dear.
        let (buy, sell) = if a.price() < b.price() { (a, b) } else { (b, a) };

        // Spend USDC on `buy` to get SOL, then sell that SOL on `sell` for USDC.
        let reserves = CycleReserves {
            a_in: buy.reserve_b,
            a_out: buy.reserve_a,
            b_in: sell.reserve_a,
            b_out: sell.reserve_b,
            fee_a_bps: buy.fee_bps,
            fee_b_bps: sell.fee_bps,
        };

        let Some(optimal) = optimal_input(&reserves) else {
            return; // genuinely no arbitrage; say nothing
        };
        let Some(gross_at_optimal) = cycle_profit(&reserves, optimal) else {
            return;
        };
        if gross_at_optimal == 0 {
            return;
        }

        let optimal_usd = optimal as f64 / 1e6;
        let tradable_usd = CAPITAL_USD - FEE_BUFFER_USD;

        // The whole point: we are capital-capped, so we take the smaller size and
        // earn correspondingly less than the maths would allow.
        let capped_usd = optimal_usd.min(tradable_usd);
        let capped_units = (capped_usd * 1e6) as u128;
        let Some(gross_units) = cycle_profit(&reserves, capped_units) else {
            return;
        };
        let gross_usd = gross_units as f64 / 1e6;

        let spread_bps = ((sell.price() / buy.price()) - 1.0) * 10_000.0;

        // Contested opportunities are the big obvious ones on major pairs; those are
        // where competitors bid the tip up to most of the profit.
        let contested = spread_bps > 12.0;
        let est_tip_usd = if contested {
            gross_usd * self.rng.range(0.50, 0.70)
        } else {
            // Uncontested: pay the floor. Median tip floor 0.0000075 SOL.
            0.0000075 * SOL_USD
        };

        let base_fee_usd = 5000.0 / LAMPORTS_PER_SOL * SOL_USD;
        let net_usd = gross_usd - est_tip_usd - base_fee_usd;

        let skipped = if net_usd <= 0.0 {
            Some("net negative after tip".to_string())
        } else if net_usd < 0.001 {
            Some("below minimum profit threshold".to_string())
        } else {
            None
        };

        let id = self.next_id;
        self.next_id += 1;

        bus.publish(Event::Opportunity {
            id,
            pair: self.pair.to_string(),
            dex_buy: buy.dex.to_string(),
            dex_sell: sell.dex.to_string(),
            spread_bps,
            optimal_size_usd: optimal_usd,
            capped_size_usd: capped_usd,
            gross_profit_usd: gross_usd,
            est_tip_usd,
            net_profit_usd: net_usd,
            contested,
            skipped_reason: skipped.clone(),
            slot: self.slot,
            ts_ms: ts,
        });

        if skipped.is_some() {
            return;
        }

        // Model the race. Contested cycles are usually lost to faster searchers;
        // uncontested ones usually land. Losing costs nothing — the bundle reverts.
        let win_p = if contested { 0.04 } else { 0.72 };
        let landed = self.rng.chance(win_p);
        let latency_ms = self.rng.range(180.0, 620.0) as u64;

        bus.publish(Event::Execution {
            id,
            opportunity_id: id,
            paper: true,
            landed,
            realised_usd: if landed { net_usd } else { 0.0 },
            tip_paid_usd: if landed { est_tip_usd } else { 0.0 },
            latency_ms,
            signature: None,
            reason: if landed {
                None
            } else {
                Some("bundle not selected — lost the race, paid nothing".into())
            },
            ts_ms: ts,
        });
    }

    pub fn status(&self, bus: &EventBus, connected: bool) {
        bus.publish(Event::Status {
            mode: "paper (simulated market)".into(),
            connected,
            slot: self.slot,
            slot_lag: 0,
            pools_tracked: self.venues.len(),
            sol_price_usd: SOL_USD,
            uptime_secs: self.started.elapsed().as_secs(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_for_a_seed() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn rng_unit_stays_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..10_000 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u), "out of range: {u}");
        }
    }

    #[test]
    fn market_emits_events_without_panicking() {
        let bus = EventBus::new();
        let mut m = Market::new(1);
        for _ in 0..500 {
            m.tick(&bus);
        }
        m.status(&bus, true);
    }

    #[test]
    fn opportunities_never_exceed_available_capital() {
        // The capital cap is the entire point of the $5 question; it must hold on
        // every emitted opportunity, not just on average.
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut m = Market::new(99);
        let mut checked = 0;

        // Drain each tick: the bus is bounded, so batching 300 ticks then reading would
        // overflow it and silently skip the events we mean to assert on.
        for _ in 0..300 {
            m.tick(&bus);
            while let Ok(ev) = rx.try_recv() {
                if let Event::Opportunity { capped_size_usd, .. } = ev {
                    assert!(
                        capped_size_usd <= CAPITAL_USD - FEE_BUFFER_USD + 1e-9,
                        "trade sized above capital: {capped_size_usd}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "simulator produced no opportunities to check");
    }

    #[test]
    fn venue_price_reflects_decimal_difference() {
        // SOL has 9 decimals, USDC has 6. Getting this wrong scales prices by 1000x.
        let v = Venue {
            dex: "test",
            reserve_a: (100.0 * 1e9) as u128,
            reserve_b: (7697.0 * 1e6) as u128,
            fee_bps: 25,
        };
        assert!((v.price() - 76.97).abs() < 0.01, "price was {}", v.price());
    }
}

#[cfg(test)]
mod diag {
    use super::*;

    #[test]
    #[ignore = "diagnostic"]
    fn report_spread_distribution() {
        let bus = EventBus::new();
        let mut m = Market::new(99);
        let mut max_bps: f64 = 0.0;
        let mut over50 = 0;
        for _ in 0..300 {
            m.tick(&bus);
            let p0 = m.venues[0].price();
            let p1 = m.venues[1].price();
            let bps = ((p0.max(p1) / p0.min(p1)) - 1.0) * 10_000.0;
            if bps > max_bps { max_bps = bps; }
            if bps > 50.0 { over50 += 1; }
        }
        println!("max spread: {max_bps:.2} bps; ticks over 50bps: {over50}/300");
        println!("final prices: {:.4} / {:.4}", m.venues[0].price(), m.venues[1].price());
    }
}
