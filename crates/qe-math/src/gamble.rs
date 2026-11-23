//! Gamble-mode target odds and bold-play stake sizing.
//!
//! Bet model (one bracket trade = one bet). The stake is the margin `m` posted
//! for the trade; the position notional is `m * leverage`. The stop-loss sits at
//! the distance that loses the whole margin, and the take-profit sits
//! `payoff_ratio` times further away, so (before costs) a win pays
//! `m * payoff_ratio` and a loss costs `m`. The round-trip cost
//! `cost_frac * notional` is charged on every bet, win or lose:
//!
//! ```text
//! win:  equity += m * payoff_ratio - cost_frac * m * leverage
//! loss: equity -= m + cost_frac * m * leverage
//! ```
//!
//! A stake is never allowed to take equity below `floor`: a loss must satisfy
//! `m * (1 + cost_frac * leverage) <= equity - floor`. Exchange limits cap the
//! notional to `[min_notional, max_notional]`; if even the minimum notional is
//! unaffordable no bet is possible and the run counts as stopped at the floor.
//!
//! Bold play (Dubins & Savage): stake exactly enough that one win reaches the
//! target, or everything above the floor if that is less. For a sub-fair coin
//! with no costs it maximises the probability of reaching the target.
//!
//! `target_odds` is a seeded Monte Carlo (SplitMix64), so identical inputs and
//! seed always give identical output.

use crate::target::SplitMix64;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::Serialize;

/// How much margin to stake per bet.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum StakePolicy {
    /// Fixed margin per bet (capped by the floor and exchange limits).
    Fixed { stake_inr: Decimal },
    /// Stake so a single win reaches the target (see [`bold_play_margin`]).
    BoldPlay,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TargetInputs {
    pub equity: Decimal,
    pub target: Decimal,
    pub floor: Decimal,
    /// Probability a bet hits take-profit (otherwise it hits stop-loss).
    pub win_p: f64,
    /// TP distance / SL distance.
    pub payoff_ratio: f64,
    /// Round-trip cost as a fraction of notional.
    pub cost_frac: f64,
    pub leverage: Decimal,
    pub bets_remaining: u32,
    pub min_notional: Decimal,
    pub max_notional: Decimal,
    pub policy: StakePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TargetOdds {
    /// P(equity reaches target within `bets_remaining` bets).
    pub p_target: f64,
    /// P(equity hits the floor, or can no longer afford a minimum bet).
    pub p_floor: f64,
    /// Mean number of bets placed per path.
    pub expected_bets: f64,
    pub median_final_equity: f64,
}

/// Relative tolerance used when comparing f64 equity against the target.
const EPS: f64 = 1e-9;

/// f64 parameters shared by the sizing rule and the simulator.
#[derive(Debug, Clone, Copy)]
struct Params {
    target: f64,
    floor: f64,
    payoff: f64,
    cost_frac: f64,
    lev: f64,
    min_notional: f64,
    max_notional: f64,
}

impl Params {
    /// Loss per unit of margin, including costs.
    fn loss_per_margin(&self) -> f64 {
        1.0 + self.cost_frac * self.lev
    }
    /// Net gain per unit of margin on a win.
    fn win_per_margin(&self) -> f64 {
        self.payoff - self.cost_frac * self.lev
    }
    /// Largest margin allowed by the floor and the max notional.
    fn cap(&self, equity: f64) -> f64 {
        ((equity - self.floor) / self.loss_per_margin()).min(self.max_notional / self.lev)
    }
    /// Apply caps and the min-notional check to a desired margin.
    fn clamp(&self, want: f64, equity: f64) -> Option<f64> {
        if want.is_nan() || want <= 0.0 || equity <= self.floor {
            return None;
        }
        let m = want.max(self.min_notional / self.lev).min(self.cap(equity));
        if m <= 0.0 || m * self.lev < self.min_notional * (1.0 - EPS) {
            return None;
        }
        Some(m)
    }
    fn bold(&self, equity: f64) -> Option<f64> {
        let w = self.win_per_margin();
        if w <= 0.0 || equity >= self.target {
            return None;
        }
        self.clamp((self.target - equity) / w, equity)
    }
}

fn f(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(0.0)
}

/// Margin to stake so a win lifts `equity` to `target`, capped so a loss cannot
/// take equity below `floor` and so the notional stays within
/// `[min_notional, max_notional]`. A required stake below the minimum notional
/// is raised to it (a win then overshoots the target). Returns `None` when no
/// bet is possible: equity at or above target, at or below floor, a win that
/// cannot cover costs, invalid leverage, or a capped stake below min notional.
/// The result is in paise (2 dp): the uncapped stake is rounded up, caps down.
pub fn bold_play_margin(
    equity: Decimal,
    target: Decimal,
    floor: Decimal,
    payoff_ratio: f64,
    leverage: Decimal,
    min_notional: Decimal,
    max_notional: Decimal,
) -> Option<Decimal> {
    bold_play_margin_with_cost(
        equity,
        target,
        floor,
        payoff_ratio,
        0.0,
        leverage,
        min_notional,
        max_notional,
    )
}

/// [`bold_play_margin`] with a round-trip cost (fraction of notional) included,
/// so the net win still reaches the target and the gross loss respects the floor.
#[allow(clippy::too_many_arguments)]
pub fn bold_play_margin_with_cost(
    equity: Decimal,
    target: Decimal,
    floor: Decimal,
    payoff_ratio: f64,
    cost_frac: f64,
    leverage: Decimal,
    min_notional: Decimal,
    max_notional: Decimal,
) -> Option<Decimal> {
    if leverage <= Decimal::ZERO || equity >= target || equity <= floor {
        return None;
    }
    let payoff = Decimal::from_f64(payoff_ratio)?;
    let cost = Decimal::from_f64(cost_frac.max(0.0))?;
    let win = payoff - cost * leverage;
    if win <= Decimal::ZERO {
        return None;
    }
    let up = RoundingStrategy::AwayFromZero;
    let down = RoundingStrategy::ToZero;
    let need = ((target - equity) / win).round_dp_with_strategy(2, up);
    let min_m = (min_notional / leverage).round_dp_with_strategy(2, up);
    let cap_floor =
        ((equity - floor) / (Decimal::ONE + cost * leverage)).round_dp_with_strategy(2, down);
    let cap_max = (max_notional / leverage).round_dp_with_strategy(2, down);
    let m = need.max(min_m).min(cap_floor).min(cap_max);
    if m <= Decimal::ZERO || m * leverage < min_notional {
        return None;
    }
    Some(m)
}

/// Seeded Monte Carlo of the gamble run: up to `bets_remaining` independent
/// bets, each won with `win_p`, stopping early at the target or the floor.
pub fn target_odds(i: &TargetInputs, sims: u32, seed: u64) -> TargetOdds {
    let e0 = f(i.equity);
    let p = Params {
        target: f(i.target),
        floor: f(i.floor),
        payoff: i.payoff_ratio,
        cost_frac: i.cost_frac.max(0.0),
        lev: f(i.leverage),
        min_notional: f(i.min_notional).max(0.0),
        max_notional: f(i.max_notional),
    };
    let sims = sims.max(1);
    let win_p = i.win_p.clamp(0.0, 1.0);
    let fixed = match i.policy {
        StakePolicy::Fixed { stake_inr } => Some(f(stake_inr)),
        StakePolicy::BoldPlay => None,
    };
    let valid = p.lev > 0.0 && p.max_notional > 0.0;
    let hit = p.target * (1.0 - EPS);
    let floor_hit = p.floor + EPS * p.target.abs().max(1.0);
    let mut rng = SplitMix64(seed);
    let (mut n_target, mut n_floor, mut bets_total) = (0u64, 0u64, 0u64);
    let mut finals = Vec::with_capacity(sims as usize);
    for _ in 0..sims {
        let mut e = e0;
        // 0 = out of bets, 1 = target, 2 = floor (or no affordable bet left)
        let mut outcome = if e >= hit { 1u8 } else { 0u8 };
        let mut b = 0u32;
        while outcome == 0 && b < i.bets_remaining {
            let m = if valid {
                match fixed {
                    Some(s) => p.clamp(s, e),
                    None => p.bold(e),
                }
            } else {
                None
            };
            let Some(m) = m else {
                outcome = 2;
                break;
            };
            b += 1;
            if rng.next_f64() < win_p {
                e += m * p.win_per_margin();
            } else {
                e -= m * p.loss_per_margin();
            }
            if e >= hit {
                outcome = 1;
            } else if e <= floor_hit {
                outcome = 2;
            }
        }
        match outcome {
            1 => n_target += 1,
            2 => n_floor += 1,
            _ => {}
        }
        bets_total += b as u64;
        finals.push(e);
    }
    let mid = finals.len() / 2;
    let (_, med, _) = finals.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    let n = sims as f64;
    TargetOdds {
        p_target: n_target as f64 / n,
        p_floor: n_floor as f64 / n,
        expected_bets: bets_total as f64 / n,
        median_final_equity: *med,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn inputs(policy: StakePolicy) -> TargetInputs {
        TargetInputs {
            equity: d("100"),
            target: d("50000"),
            floor: d("20"),
            win_p: 0.45,
            payoff_ratio: 2.0,
            cost_frac: 0.0015,
            leverage: d("20"),
            bets_remaining: 200,
            min_notional: d("500"),
            max_notional: d("10000000"),
            policy,
        }
    }

    #[test]
    fn bold_play_reaches_target_in_one_win_when_affordable() {
        // need (200 - 150) / 1 = 50 margin; floor 0 leaves 150 available
        let m = bold_play_margin(
            d("150"),
            d("200"),
            d("0"),
            1.0,
            d("10"),
            d("0"),
            d("1000000"),
        )
        .unwrap();
        assert_eq!(m, d("50"));
        // with costs the net win still reaches the target, by under 5 paise
        let m = bold_play_margin_with_cost(
            d("150"),
            d("200"),
            d("0"),
            2.0,
            0.0015,
            d("10"),
            d("0"),
            d("1000000"),
        )
        .unwrap();
        let net = m * d("2") - m * d("10") * d("0.0015");
        assert!(d("150") + net >= d("200"), "{m} {net}");
        assert!(d("150") + net - d("200") < d("0.05"));
    }

    #[test]
    fn bold_play_respects_floor_and_notional_limits() {
        let big = d("1000000000");
        // floor caps: equity 100, floor 40 -> at most 60 at risk
        let m = bold_play_margin(d("100"), d("50000"), d("40"), 2.0, d("20"), d("0"), big).unwrap();
        assert_eq!(m, d("60"));
        // max notional caps margin to 2000/20 = 100
        let m = bold_play_margin(
            d("1000"),
            d("50000"),
            d("0"),
            2.0,
            d("20"),
            d("0"),
            d("2000"),
        )
        .unwrap();
        assert_eq!(m, d("100"));
        // tiny need is raised to min notional (500/20 = 25)
        let m = bold_play_margin(d("199"), d("200"), d("0"), 1.0, d("20"), d("500"), big).unwrap();
        assert_eq!(m, d("25"));
        // min notional unaffordable above floor -> None
        assert_eq!(
            bold_play_margin(d("30"), d("200"), d("20"), 1.0, d("20"), d("500"), big),
            None
        );
        // at floor, at target, bad leverage -> None
        assert_eq!(
            bold_play_margin(d("20"), d("200"), d("20"), 1.0, d("20"), d("0"), big),
            None
        );
        assert_eq!(
            bold_play_margin(d("200"), d("200"), d("0"), 1.0, d("20"), d("0"), big),
            None
        );
        assert_eq!(
            bold_play_margin(d("100"), d("200"), d("0"), 1.0, d("0"), d("0"), big),
            None
        );
    }

    #[test]
    fn decimal_and_f64_rules_agree() {
        let p = Params {
            target: 50000.0,
            floor: 20.0,
            payoff: 2.0,
            cost_frac: 0.0015,
            lev: 20.0,
            min_notional: 500.0,
            max_notional: 1e7,
        };
        for eq in ["30", "100", "777.77", "20000", "49999"] {
            let dm = bold_play_margin_with_cost(
                d(eq),
                d("50000"),
                d("20"),
                2.0,
                0.0015,
                d("20"),
                d("500"),
                d("10000000"),
            );
            let fm = p.bold(f(d(eq)));
            match (dm, fm) {
                (Some(a), Some(b)) => assert!((f(a) - b).abs() < 0.011, "{eq}: {a} vs {b}"),
                (None, None) => {}
                other => panic!("{eq}: {other:?}"),
            }
        }
    }

    #[test]
    fn monte_carlo_probabilities_are_valid() {
        for pol in [
            StakePolicy::BoldPlay,
            StakePolicy::Fixed { stake_inr: d("30") },
        ] {
            let o = target_odds(&inputs(pol), 5000, 7);
            assert!((0.0..=1.0).contains(&o.p_target), "{o:?}");
            assert!((0.0..=1.0).contains(&o.p_floor), "{o:?}");
            assert!(o.p_target + o.p_floor <= 1.0 + 1e-12);
            assert!(o.expected_bets >= 1.0 && o.expected_bets <= 200.0);
            assert!(o.median_final_equity.is_finite());
        }
        // already at target: no bets, certain
        let mut i = inputs(StakePolicy::BoldPlay);
        i.equity = d("50000");
        let o = target_odds(&i, 100, 1);
        assert_eq!((o.p_target, o.expected_bets), (1.0, 0.0));
        // certain loss: always floor
        let mut i = inputs(StakePolicy::BoldPlay);
        i.win_p = 0.0;
        let o = target_odds(&i, 100, 1);
        assert_eq!((o.p_target, o.p_floor), (0.0, 1.0));
    }

    #[test]
    fn fixed_stake_never_breaches_floor() {
        let mut i = inputs(StakePolicy::Fixed { stake_inr: d("50") });
        i.win_p = 0.2;
        let o = target_odds(&i, 2000, 3);
        assert!(o.median_final_equity >= 20.0 - 1e-6, "{o:?}");
        assert!(o.p_floor > 0.9, "{o:?}");
    }

    /// Fair coin, even payoff, no costs/limits: bold play reaches the target
    /// with probability equity/target. Sub-fair coin from 1/4 of the target:
    /// two straight wins are needed, so P = p^2.
    #[test]
    fn bold_play_matches_closed_form() {
        let base = TargetInputs {
            equity: d("30"),
            target: d("100"),
            floor: d("0"),
            win_p: 0.5,
            payoff_ratio: 1.0,
            cost_frac: 0.0,
            leverage: d("1"),
            bets_remaining: 200,
            min_notional: d("0"),
            max_notional: d("1000000000000"),
            policy: StakePolicy::BoldPlay,
        };
        let o = target_odds(&base, 20_000, 11);
        assert!((o.p_target - 0.30).abs() < 0.015, "{o:?}");
        assert!((o.p_target + o.p_floor - 1.0).abs() < 1e-3, "{o:?}");

        let mut q = base.clone();
        q.equity = d("25");
        q.win_p = 0.4;
        let o = target_odds(&q, 20_000, 12);
        assert!((o.p_target - 0.16).abs() < 0.012, "{o:?}");
        assert!(o.expected_bets <= 2.0);
    }

    #[test]
    fn deterministic_by_seed() {
        let i = inputs(StakePolicy::BoldPlay);
        assert_eq!(target_odds(&i, 3000, 99), target_odds(&i, 3000, 99));
        let j = inputs(StakePolicy::Fixed { stake_inr: d("25") });
        assert_eq!(target_odds(&j, 3000, 5), target_odds(&j, 3000, 5));
    }

    #[test]
    fn default_sims_are_fast() {
        // 20k sims must take < 200 ms in release; allow slack for debug builds.
        let mut i = inputs(StakePolicy::Fixed { stake_inr: d("25") });
        i.win_p = 0.5;
        i.bets_remaining = 100;
        let t = std::time::Instant::now();
        let _ = target_odds(&i, 20_000, 1);
        let ms = t.elapsed().as_millis();
        let budget = if cfg!(debug_assertions) { 3000 } else { 200 };
        assert!(ms < budget, "{ms} ms");
    }
}
