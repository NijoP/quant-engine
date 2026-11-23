# Forecast calibration (`crates/qe-research`)

Status: **no forecast in the desk is calibrated for strategy authority.** The numbers below
come from real public CoinDCX data recorded and downloaded on 2026-11-23, and they show no
exploitable edge after costs. They are reported as measured.

## What exists

| Piece | Command / API | Verified by |
|---|---|---|
| Live recorder (public only) | `qe-research record --out DIR --minutes 40 --focus 10` | ran 40 min live, 0 errors |
| Candle history loader | `qe-research history --out DIR --pairs 30 --days 7` | 30 pairs, 10 080 × 1m each, 5m..3D, local 2D |
| Walk-forward evaluator | `qe-research evaluate --rec DIR --hist DIR --out calibration.json` | unit tests + real run below |
| Calibration-status API | `qe_research::calibration::CalibrationReport` | unit tests |

The store (`DIR`) is local and gitignored (`/data/`). Nothing private is recorded: no key is
read and only public endpoints are called (RT prices, depth-20 order books, public trades,
instrument metadata and candlesticks), all through the shared `PublicClient` limiter.

### Recorder
* `rt.jsonl`: every RT poll (about 1/s) of `market_data/v3/current_prices/futures/rt`, restricted
  to pairs in the active INR + USDT futures universe. It stores last, mark and 24 h volume,
  with exchange `ts`/`vs` and local `recv_ms`.
* `books.jsonl`: depth-20 snapshots for the 10 most liquid USDT pairs (about 3 books/s
  round-robin). Crossed or empty books are dropped at load.
* `trades.jsonl`: recent public trades per focus pair (recorded but not yet used by the
  evaluator), plus `instruments.jsonl` with live taker fees.

### Historical loader
Resolutions were checked against the live endpoint on 2026-11-23: `1, 5, 15, 30, 60, 240,
480, 1D, 3D, 1M` answer `s=ok`; `2D` and `1W` return nothing. One request returns at most
30 000 candles, starting at `from`, so 1m history is fetched in windows of up to 20 000
minutes that end now. A candle is `complete` only when `open + width <= fetch time`. 2d is
built locally with `qe_math::timeframe::aggregate_2d`, which takes only two completed,
contiguous days.

### Model under test
`desk.fp.v1` reproduces `qe-desk::evaluate` exactly, using its uncalibrated priors
λ = 0.2, γ = 0.05, k_s = 1.0, k_t = 1.5 and lattice dt = 0.5 s:

* σ₁ₛ = RV(1-step log returns of the last ≤ 600 RT samples) / √(mean Δt)
* μ = λ·r₆₀/60 + γ·OBI₁₀·σ₁ₛ
* TP = k_t·σ₁ₛ·√120 and SL = k_s·σ₁ₛ·√120
* P(TP first) and P(SL first) come from `prob::first_passage`; P(up at h) = Φ(μh / σ√h)
* C = 2·taker + spread + 2 × 1 bp

`mom.fp.v1` is the same model with OBI = 0, which lets it run on all 505 pairs.
`mom.fp.v1.1m` estimates σ from the last 60 completed 1m returns, takes r₆₀ as the last 1m
return, and checks barriers on candle high/low.

### No-lookahead protocol
* Features at decision time t use only data with ts ≤ t. A book counts only if ts ≤ t and
  it is at most 5 s old.
* Labels use only the path in (t, t + 120 s], which must cover at least t + 115 s.
* Take-profit realises exactly TP, as a resting limit. A stop realises the observed price at
  the touching sample, so gap risk is included. When TP and SL fall inside the same 1m candle
  the order is unknown, and the stop is assumed to come first.
* `up_h` is the sign of the move at the first sample ≥ t + h. Ties (no price change) are
  excluded, and their share is reported.
* Walk-forward uses 5 chronological folds. Fold 0 only trains. For fold k, both the
  histogram-binning recalibration (10 bins, shrunk toward the training base rate) and the
  climatology baseline are fitted on points whose label window ends before fold k starts
  (a 120 s purge). Out-of-sample metrics aggregate folds 1–4.
* Decisions are taken every 5 s on RT data and every 1 m on candles. Their 120 s outcomes
  overlap, so the report also gives an approximate count of non-overlapping windows.
  Treat sample sizes as optimistic.

### Status rule and authority
Each (model, target) pair is graded:
* `uncalibrated`: no out-of-sample observations.
* `calibrating`: a gate fails, and the reasons are listed.
* `calibrated`: all of these hold:
  * n_oos ≥ 1000 over at least 3 OOS folds;
  * ECE of the recalibrated mapping ≤ 0.03;
  * Brier skill against climatology ≥ 0.01;
  * skill > 0 in at least 2/3 of the folds.

`CalibrationReport::grants_authority(model, target)` is true only for `calibrated`.
`recalibrate()` returns a probability only in that case. A missing report means
everything is uncalibrated. **s2-desk must deny strategy authority to any forecast for
which `grants_authority` is false.** Calibration is a necessary condition for authority,
not a sufficient one: the risk governor still requires positive net EV after costs.

## Results (2026-11-23)

<!-- RESULTS -->

## Not done / next
* Trade-tape features (aggressor flow and intensity) are recorded but not yet scored.
  The WebSocket feed (s2-ws) will replace 1 s RT polling for barrier monitoring.
* No parameter search was run. The desk priors are scored as-is. Any (λ, γ, k_s, k_t)
  search must use the same walk-forward split, so the search itself isn't fitted in-sample.
* Book-dependent results cover about 40 minutes of 10 pairs, which is far too short to
  claim calibration of `desk.fp.v1`. Keep recording.
