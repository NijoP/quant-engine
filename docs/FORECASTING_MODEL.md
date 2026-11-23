# Forecasting model (`qe-math`)

Status: **partially implemented; nothing here is calibrated.** No component may be given
strategy authority until it is calibrated on chronological, out-of-sample data.

Implemented and unit-tested:
* First-passage probabilities within 120 s for drifted Brownian motion (deterministic lattice,
  `prob::first_passage`): P(TP first), P(SL first), P(neither), conditional hit times,
  E[return | neither].
* Bracket expected value without double-counting fees (`prob::bracket_ev`).
* Scoring rules: Brier score, log loss, expected calibration error (`prob`).
* Seeded stochastic target simulation (`target::simulate`).
* Distribution primitives: tail-accurate Φ, ln Φ, Φ⁻¹, sigmoid (`dist`).
* Inputs: the unified `features::FeatureVector` (σ₁, σ_h, momentum z, OBI, OFI, TFI, intensity,
  context per timeframe).

Not started (planned interface): per-horizon (1, 2, 5, 10, 15, 30, 60, 90, 120 s) forecasts
from EW drift, vol-adjusted momentum, logistic, Kalman local-trend and Bayesian Normal–Normal
models; ensemble with model-disagreement uncertainty; a calibration-status flag that withholds
full authority from uncalibrated models; walk-forward splits with no lookahead.

## Sprint 3: challengers (`qe_math::learn`, `qe-research challenge`)

Implemented and unit-tested in `qe_math::learn`:
* `Logistic`: ridge logistic regression on standardized features, fitted by Newton/IRLS.
  The intercept is not penalised, and coefficients stay on the standardized scale so they
  can be read. Fitting refuses degenerate input: fewer than 20 rows, or one class only.
* `Isotonic`: monotone recalibration by pool-adjacent-violators. Each block's frequency is
  shrunk toward the training base rate, then monotonicity is re-imposed.
* `logit(p)`: the Platt-scaling input.

Challengers scored by `qe-research challenge` (versioned cards in `crates/qe-research/models.json`):

| id | version | model |
|---|---|---|
| `desk.fp.v1` | v1 | **champion**. μ = λ·r₆₀/60 + γ·OBI₁₀·σ, σ from RT, first-passage lattice; uncalibrated priors |
| `desk.fp.v1+iso` | v1+iso.1 | champion probabilities with isotonic recalibration fitted on past folds |
| `desk.fp.v1+platt` | v1+platt.1 | champion probabilities with Platt scaling fitted on past folds |
| `logit.flow.v2` | v2 | one logistic model per target on the 11 order-flow features below |
| `cond.flow.v2` | v2 | conditional-historical frequency in 18 cells: OBI₅ (< −0.2, mid, > 0.2) × TFI₃₀ (same cuts) × vol regime (≥ 0 or < 0), with Beta(30) shrinkage to the base rate |

Features at decision time t use only data with ts ≤ t. The book must be valid (not
crossed or empty) and at most 5 s old.

| feature | definition |
|---|---|
| `mom60_z`, `mom10_z` | ln(P_t / P_{t−L}) / (σ₁ₛ √L), L = 60 s, 10 s |
| `obi1`, `obi5`, `obi20` | depth imbalance (Σq_b − Σq_a)/(Σq_b + Σq_a) over 1, 5, 20 levels |
| `micro_dev_z` | (microprice − mid) / mid / σ₁ₛ, with microprice = (a·q_b + b·q_a)/(q_b + q_a) |
| `ofi_z` | Cont best-level OFI between the two latest valid snapshots (gap ≤ 5 s), divided by mean best-level depth and clamped to ±10. Snapshots are treated as snapshots, never as deltas |
| `tfi30` | aggressor-signed quantity imbalance over (t − 30 s, t] (`is_maker` = buyer was maker ⇒ sell aggressor; this agrees with the tick rule on 93 % of price changes in the recording) |
| `intensity` | ln(1 + n₃₀) − ln(1 + n₃₀₀/10): trade-rate surge against the 5-minute rate |
| `vol_regime` | ln(σ over the last 60 RT samples / σ over the last 600) |
| `spread_bps` | (ask₁ − bid₁)/mid × 10⁴ |

The trading rule is the same for every model. For each side:
EV = p_TP·TP − p_SL·L̄_SL + (1 − p_TP − p_SL)⁺·R̄_neither − C. Here L̄_SL (the realised
stop loss, gaps included) and R̄_neither come from training folds only. The side with the
larger EV is taken when EV > threshold, with at most one open bracket per pair.

Assumptions and limitations are listed on each card in the registry. The main ones:
* the tape is the union of polled REST trades, so it can miss trades;
* books are REST snapshots sampled every 2–3 s per pair, not the full stream;
* maker scenarios ignore queue position, non-fills and adverse selection.

Results and the promotion decision are in docs/CALIBRATION.md.
