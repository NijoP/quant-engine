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
