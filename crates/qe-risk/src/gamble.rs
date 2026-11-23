//! Gamble mode: an owner-armed execution profile that bets without waiting for
//! the forecast to prove a calibrated edge (docs/GAMBLE_MODE.md).
//!
//! What gamble mode skips: the calibration-authority gate, the net-EV > 0 gate
//! and the session-loss / high-water-mark stops. What it keeps or adds: an
//! owner-set stake (INR), leverage (capped at the exchange maximum) and balance
//! floor (a permanent stop), one open position, mandatory SL and TP, the 120 s
//! time exit, the kill switch, data-integrity and instrument checks, and no
//! martingale (a bet after a loss is never larger than the losing bet).
//!
//! Everything here is pure and deterministic: no I/O, no clock. The target
//! odds Monte Carlo lives in `qe_math::gamble`.
// `!(x > 0.0)` is deliberate: NaN must fail every check.
#![allow(clippy::neg_cmp_op_on_partial_ord)]
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub mod payoff;

/// Text the owner must copy into `configs/gamble.toml` as `owner_ack`.
pub const OWNER_ACK: &str =
    "I ACCEPT: gamble mode has no proven edge and can lose my stake down to the floor";

/// How each bet is sized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StakePolicy {
    /// Margin per bet = the owner's stake (never more than equity above the floor).
    Fixed,
    /// Bold play: the largest bet the floor, leverage cap and exchange limits
    /// allow, unless a smaller bet reaches the target with one win. Maximises
    /// P(reach a fixed target) when each bet has negative expectation.
    TargetOptimal,
}

impl StakePolicy {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "fixed" => Ok(Self::Fixed),
            "target_optimal" | "bold" => Ok(Self::TargetOptimal),
            o => Err(format!(
                "stake_policy {o:?} unknown (use \"fixed\" or \"target_optimal\")"
            )),
        }
    }
}

/// Owner-set gamble parameters. Money is Decimal at the boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GambleParams {
    /// Margin committed per bet under [`StakePolicy::Fixed`] (INR).
    pub stake_inr: Decimal,
    /// Owner leverage; the exchange maximum for the side and tier caps it.
    pub leverage: u32,
    /// Below this equity (INR) gamble mode stops until the owner re-arms.
    pub floor_inr: Decimal,
    pub stake_policy: StakePolicy,
}

impl GambleParams {
    pub fn stake(&self) -> f64 {
        self.stake_inr.to_f64().unwrap_or(0.0)
    }
    pub fn floor(&self) -> f64 {
        self.floor_inr.to_f64().unwrap_or(f64::INFINITY)
    }
}

/// The raw config values as read from TOML (any may be missing).
#[derive(Debug, Clone, Default)]
pub struct RawGamble {
    pub enabled: Option<bool>,
    pub owner_ack: Option<String>,
    pub stake_inr: Option<String>,
    pub leverage: Option<String>,
    pub floor_inr: Option<String>,
    pub stake_policy: Option<String>,
}

fn money(name: &str, v: &Option<String>) -> Result<Decimal, String> {
    let s = v
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{name} is not set: gamble mode has no trading defaults"))?;
    Decimal::from_str(s).map_err(|e| format!("{name} = {s:?} is not a decimal amount: {e}"))
}

/// Validate the owner's config. Refuses (Err) unless gamble mode is enabled,
/// acknowledged, and stake, leverage and floor are all set and sane for the
/// starting equity. There are no defaults for stake, leverage or floor.
pub fn validate(raw: &RawGamble, start_equity: f64) -> Result<GambleParams, String> {
    if raw.enabled != Some(true) {
        return Err("gamble mode is off: set enabled = true in configs/gamble.toml".into());
    }
    if raw.owner_ack.as_deref().map(str::trim) != Some(OWNER_ACK) {
        return Err(format!(
            "owner_ack missing or wrong: the owner must set owner_ack = {OWNER_ACK:?}"
        ));
    }
    let stake = money("stake_inr", &raw.stake_inr)?;
    let floor = money("floor_inr", &raw.floor_inr)?;
    let lev_s = raw
        .leverage
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("leverage is not set: gamble mode has no trading defaults")?;
    let leverage: u32 = lev_s
        .parse()
        .map_err(|_| format!("leverage = {lev_s:?} must be a whole number >= 1"))?;
    if leverage < 1 {
        return Err("leverage must be >= 1".into());
    }
    if stake <= Decimal::ZERO {
        return Err(format!("stake_inr {stake} must be > 0"));
    }
    if floor < Decimal::ZERO {
        return Err(format!("floor_inr {floor} must be >= 0"));
    }
    let stake_policy = match raw.stake_policy.as_deref() {
        None => StakePolicy::Fixed,
        Some(s) => StakePolicy::parse(s)?,
    };
    let p = GambleParams {
        stake_inr: stake,
        leverage,
        floor_inr: floor,
        stake_policy,
    };
    if !(start_equity > p.floor()) {
        return Err(format!(
            "equity {start_equity} is not above floor_inr {floor}: nothing to bet"
        ));
    }
    if p.stake_policy == StakePolicy::Fixed && p.stake() > start_equity - p.floor() {
        return Err(format!(
            "stake_inr {stake} exceeds equity {start_equity} minus floor {floor}"
        ));
    }
    Ok(p)
}

/// Inputs to size one bet, all in account currency (INR) except fractions.
#[derive(Debug, Clone, Copy)]
pub struct SizeCtx {
    pub equity: f64,
    pub available_margin: f64,
    /// Effective leverage (owner leverage capped by the exchange for this tier).
    pub leverage: f64,
    /// Round-trip cost as a fraction of notional (fees + spread + slippage).
    pub cost_frac: f64,
    /// Net win as a fraction of notional if TP is hit (tp - cost).
    pub win_frac: f64,
    pub target: f64,
    /// Notional of the previous bet if it lost (no size increase after a loss).
    pub after_loss_cap: Option<f64>,
    pub min_notional: f64,
    pub max_notional: f64,
}

/// Notional (INR) for one bet, or why none fits. Isolated margin caps the worst
/// loss at the margin, so margin plus round-trip fees must fit above the floor:
/// margin * (1 + L * cost) <= equity - floor. The floor therefore holds even if
/// the stop is gapped through. A bet is never raised to meet the exchange
/// minimum beyond what that inequality allows.
pub fn bet_notional(p: &GambleParams, c: &SizeCtx) -> Result<f64, String> {
    let room = c.equity - p.floor();
    if !(room > 0.0) {
        return Err(format!(
            "equity {:.4} at or below floor {:.4}",
            c.equity,
            p.floor()
        ));
    }
    let lev = c.leverage.max(1.0);
    let margin_cap = (room / (1.0 + lev * c.cost_frac.max(0.0)))
        .min(c.available_margin.max(0.0))
        .min(c.equity.max(0.0));
    let mut n = match p.stake_policy {
        StakePolicy::Fixed => p.stake().min(margin_cap) * lev,
        StakePolicy::TargetOptimal => {
            let mut n = margin_cap * lev;
            if c.win_frac > 0.0 && c.target > c.equity {
                // one win is enough: do not bet more than needed
                n = n.min(((c.target - c.equity) / c.win_frac).max(c.min_notional));
            }
            n
        }
    };
    if let Some(cap) = c.after_loss_cap {
        n = n.min(cap);
    }
    n = n.min(c.max_notional);
    if !(n >= c.min_notional && n > 0.0) {
        return Err(format!(
            "largest bet above the floor (notional INR {n:.4}) is below the exchange minimum INR {:.4}",
            c.min_notional
        ));
    }
    Ok(n)
}

/// Per-key empirical counts: forward-resolved samples or realised bets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct HitCounts {
    pub n: u32,
    pub hits: u32,
    /// Sum of the model probability over the same samples.
    pub sum_p: f64,
}

/// Shrinkage strength (pseudo-samples) for the per-instrument/side and the
/// realised-bet layers.
pub const K_LOCAL: f64 = 30.0;
pub const K_TRADES: f64 = 10.0;
pub const GLOBAL_MIN_N: u32 = 50;

/// Best available estimate of P(TP first within 120 s) for one bet:
/// 1. global recalibration: scale the model probability by the forward
///    monitor's measured hit rate / mean predicted probability (n >= 50);
/// 2. empirical-Bayes shrink toward the hit rate of resolved forward samples
///    for this instrument and side (prior weight [`K_LOCAL`]);
/// 3. shrink toward realised gamble bets on this instrument and side
///    (prior weight [`K_TRADES`]).
pub fn win_prob(p_model: f64, global: HitCounts, local: HitCounts, trades: HitCounts) -> f64 {
    let mut p = p_model.clamp(0.0, 1.0);
    if global.n >= GLOBAL_MIN_N && global.sum_p > 0.0 {
        p *= global.hits as f64 / global.sum_p;
    }
    p = p.clamp(0.001, 0.999);
    p = (local.hits as f64 + K_LOCAL * p) / (local.n as f64 + K_LOCAL);
    p = (trades.hits as f64 + K_TRADES * p) / (trades.n as f64 + K_TRADES);
    p.clamp(0.001, 0.999)
}

/// One candidate bet, as fractions of notional.
#[derive(Debug, Clone, Serialize)]
pub struct BetEstimate {
    pub pair: String,
    pub long: bool,
    pub ts_ms: i64,
    pub p_model: f64,
    /// Estimated P(TP first) after recalibration and shrinkage.
    pub p_win: f64,
    pub p_stop: f64,
    pub p_neither: f64,
    pub tp_frac: f64,
    pub sl_frac: f64,
    pub cost_frac: f64,
    /// Net win if TP: tp - cost.
    pub win_frac: f64,
    /// Net loss if SL: sl + cost.
    pub loss_frac: f64,
    /// Net if neither barrier is hit (time exit).
    pub neither_frac: f64,
    /// Expected net return per unit notional.
    pub ev_frac: f64,
    /// Ranking score: EV per unit of stop loss.
    pub score: f64,
}

impl BetEstimate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pair: &str,
        long: bool,
        ts_ms: i64,
        p_model: f64,
        p_win: f64,
        p_neither_model: f64,
        e_ret_neither: f64,
        tp_frac: f64,
        sl_frac: f64,
        cost_frac: f64,
    ) -> Self {
        let p_neither = p_neither_model.clamp(0.0, 1.0 - p_win);
        let p_stop = (1.0 - p_win - p_neither).max(0.0);
        let win = tp_frac - cost_frac;
        let loss = sl_frac + cost_frac;
        let neither = e_ret_neither - cost_frac;
        let ev = p_win * win - p_stop * loss + p_neither * neither;
        Self {
            pair: pair.into(),
            long,
            ts_ms,
            p_model,
            p_win,
            p_stop,
            p_neither,
            tp_frac,
            sl_frac,
            cost_frac,
            win_frac: win,
            loss_frac: loss,
            neither_frac: neither,
            ev_frac: ev,
            score: if loss > 0.0 {
                ev / loss
            } else {
                f64::NEG_INFINITY
            },
        }
    }

    /// Structural reasons never to take this bet, whatever the alternatives.
    pub fn hopeless(&self) -> Option<String> {
        if !(self.win_frac > 0.0) {
            return Some(format!(
                "TP {:.2} bps does not cover round-trip cost {:.2} bps",
                self.tp_frac * 1e4,
                self.cost_frac * 1e4
            ));
        }
        if !self.score.is_finite() {
            return Some("bet estimate not computable".into());
        }
        None
    }
}

/// Pick among fresh candidates: the highest score wins; a candidate is skipped
/// when it is not the best available (or is hopeless). Returns the reason to
/// skip `me`, or None to bet.
pub fn skip_reason(me: &BetEstimate, board: &[BetEstimate]) -> Option<String> {
    if let Some(h) = me.hopeless() {
        return Some(format!("gamble: skipped, {h}"));
    }
    let best = board
        .iter()
        .filter(|b| b.hopeless().is_none() && !(b.pair == me.pair && b.long == me.long))
        .max_by(|a, b| a.score.total_cmp(&b.score));
    match best {
        Some(b) if b.score > me.score => Some(format!(
            "gamble: skipped, not the best available bet (score {:.4} < {:.4} on {} {})",
            me.score,
            b.score,
            b.pair,
            if b.long { "long" } else { "short" }
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> RawGamble {
        RawGamble {
            enabled: Some(true),
            owner_ack: Some(OWNER_ACK.into()),
            stake_inr: Some("10".into()),
            leverage: Some("10".into()),
            floor_inr: Some("50".into()),
            stake_policy: None,
        }
    }

    #[test]
    fn refuses_unless_every_owner_param_is_set() {
        assert!(validate(&raw(), 100.0).is_ok());
        let unset: [fn(&mut RawGamble); 4] = [
            |r| r.stake_inr = None,
            |r| r.leverage = None,
            |r| r.floor_inr = None,
            |r| r.stake_inr = Some(" ".into()),
        ];
        for f in unset {
            let mut r = raw();
            f(&mut r);
            let e = validate(&r, 100.0).unwrap_err();
            assert!(e.contains("not set"), "{e}");
        }
        let mut r = raw();
        r.enabled = None;
        assert!(validate(&r, 100.0).unwrap_err().contains("off"));
        let mut r = raw();
        r.enabled = Some(false);
        assert!(validate(&r, 100.0).is_err());
        let mut r = raw();
        r.owner_ack = Some("yes".into());
        assert!(validate(&r, 100.0).unwrap_err().contains("owner_ack"));
    }

    #[test]
    fn refuses_nonsense_values() {
        let bad: [fn(&mut RawGamble); 6] = [
            |r| r.leverage = Some("0".into()),
            |r| r.leverage = Some("2.5".into()),
            |r| r.stake_inr = Some("-1".into()),
            |r| r.floor_inr = Some("100".into()),
            |r| r.stake_inr = Some("60".into()),
            |r| r.stake_policy = Some("martingale".into()),
        ];
        for f in bad {
            let mut r = raw();
            f(&mut r);
            assert!(validate(&r, 100.0).is_err());
        }
        let mut r = raw();
        r.stake_policy = Some("target_optimal".into());
        assert_eq!(
            validate(&r, 100.0).unwrap().stake_policy,
            StakePolicy::TargetOptimal
        );
        assert_eq!(
            validate(&raw(), 100.0).unwrap().stake_inr,
            Decimal::new(10, 0)
        );
    }

    fn ctx(eq: f64) -> SizeCtx {
        SizeCtx {
            equity: eq,
            available_margin: eq,
            leverage: 10.0,
            cost_frac: 0.0015,
            win_frac: 0.0035,
            target: 50_000.0,
            after_loss_cap: None,
            min_notional: 6.0,
            max_notional: 1e9,
        }
    }

    #[test]
    fn fixed_stake_is_capped_above_the_floor() {
        let p = validate(&raw(), 100.0).unwrap();
        assert!((bet_notional(&p, &ctx(100.0)).unwrap() - 100.0).abs() < 1e-9);
        // 5 INR above the floor: margin * (1 + 10 * 0.0015) <= 5
        let n = bet_notional(&p, &ctx(55.0)).unwrap();
        assert!((n - 10.0 * 5.0 / 1.015).abs() < 1e-9);
        // worst case (full margin + fees) still leaves equity >= floor
        assert!(55.0 - n / 10.0 - n * 0.0015 >= 50.0 - 1e-9);
        assert!(bet_notional(&p, &ctx(50.0)).is_err());
        assert!(bet_notional(&p, &ctx(50.5))
            .unwrap_err()
            .contains("minimum"));
    }

    #[test]
    fn no_size_increase_after_a_loss() {
        let mut r = raw();
        r.stake_policy = Some("target_optimal".into());
        let p = validate(&r, 100.0).unwrap();
        let mut c = ctx(100.0);
        let free = bet_notional(&p, &c).unwrap();
        c.after_loss_cap = Some(123.0);
        assert_eq!(bet_notional(&p, &c).unwrap(), 123.0);
        assert!(free > 123.0);
    }

    #[test]
    fn bold_play_bets_max_or_just_enough() {
        let mut r = raw();
        r.stake_policy = Some("target_optimal".into());
        let p = validate(&r, 100.0).unwrap();
        let n = bet_notional(&p, &ctx(100.0)).unwrap();
        assert!((n - 10.0 * 50.0 / 1.015).abs() < 1e-6, "{n}");
        // near the target: only what one win needs
        let n = bet_notional(&p, &ctx(49_999.0)).unwrap();
        assert!((n - 1.0 / 0.0035).abs() < 1e-6);
    }

    #[test]
    fn win_prob_recalibrates_and_shrinks() {
        let none = HitCounts::default();
        assert!((win_prob(0.2, none, none, none) - 0.2).abs() < 1e-12);
        // model over-predicts: mean P 0.155 vs hit rate 0.116 (the 30-min run)
        let g = HitCounts {
            n: 1000,
            hits: 116,
            sum_p: 155.0,
        };
        assert!((win_prob(0.155, g, none, none) - 0.116).abs() < 1e-9);
        // a pair/side that resolved 0 of 30 is pulled halfway to 0
        let l = HitCounts {
            n: 30,
            hits: 0,
            sum_p: 4.0,
        };
        assert!((win_prob(0.2, none, l, none) - 0.1).abs() < 1e-9);
        let t = HitCounts {
            n: 10,
            hits: 10,
            sum_p: 2.0,
        };
        assert!((win_prob(0.2, none, none, t) - 0.6).abs() < 1e-9);
    }

    fn est(pair: &str, p: f64) -> BetEstimate {
        BetEstimate::new(pair, true, 0, p, p, 0.3, 0.0, 0.005, 0.0033, 0.0015)
    }

    #[test]
    fn only_the_best_bet_is_taken() {
        let a = est("A", 0.30);
        let b = est("B", 0.40);
        let board = vec![a.clone(), b.clone()];
        assert!(skip_reason(&a, &board).unwrap().contains("not the best"));
        assert_eq!(skip_reason(&b, &board), None);
        // a TP that cannot pay the costs is never taken
        let bad = BetEstimate::new("C", true, 0, 0.9, 0.9, 0.0, 0.0, 0.001, 0.001, 0.0015);
        assert!(skip_reason(&bad, std::slice::from_ref(&bad))
            .unwrap()
            .contains("cost"));
        // EV = p W - p_s L + p_n N
        let want = 0.3 * 0.0035 - 0.4 * 0.0048 + 0.3 * -0.0015;
        assert!((a.ev_frac - want).abs() < 1e-12);
    }
}
