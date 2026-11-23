# Forecast calibration (`crates/qe-research`)

Status: **no forecast in the desk is calibrated for strategy authority.** The numbers below
come from real public CoinDCX data recorded and downloaded on 2026-11-23, and they show no
exploitable edge after costs. They are reported as measured.

## What exists

| Piece | Command / API | Verified by |
|---|---|---|
| Live recorder (public only) | `qe-research record --out DIR --minutes 40 --focus 10` | ran 40 min live: 2 032 RT polls, 5 916 books, 1 101 trade polls, 6 transient errors (HTTP 503 on books) |
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

Data:
* A 40.0-minute live recording (15:56–16:36 IST): 2 032 RT polls over all 505 active
  futures pairs and 5 916 depth-20 books on the 10 most liquid USDT pairs.
* 7 days of completed 1m candles for the 30 most liquid USDT pairs.

The full machine-readable report is `crates/qe-research/calibration.json`, which s2-desk loads.
Reproduce it with `qe-research evaluate --rec <store> --hist <store> --out calibration.json`.

**Headline (honest):**
1. **No (model, target) pair is `calibrated`. Every entry is `calibrating`**, so the desk must
   withhold strategy authority from all of them.
2. **The raw desk probabilities are miscalibrated.** Raw ECE for P(TP first) and P(SL first)
   is 0.05–0.19. The model predicts SL-first at about 0.34 where 0.15–0.31 is realised.
   Raw Brier skill against climatology is negative on the two large datasets: −0.03 to −0.34.
3. **Recalibration rescues calibration but not information.** Binning fitted only on past
   folds brings ECE down to 0.002–0.09. Its best Brier skill, though, is 0.002–0.010 (all
   < 0.01): almost nothing beyond the base rate.
4. **There is no edge after costs.** The measured round-trip cost is 13.4 bps (2 × taker
   0.059 % + spread + 2 × 1 bp). The unconditional realised gross return of a 120 s bracket
   is about 0 bps (−0.14 to +0.25) on every dataset. At 1× cost the desk model selects 4 EV>0
   trades on RT data, realising −8.9 bps each. On 1m history it selects 762, realising
   −13.6 bps each.
5. **EV is overstated.** Even at zero cost, EV>0 selections realise +0.18 to +0.26 bps against
   a predicted +1.6 to +2.2 bps, roughly 8–10× too optimistic. The forecast's direction
   signal is close to noise.
6. Consequence for the target controller: the measured per-trade expectancy is negative at
   real costs. No trade rate makes INR 100 → 18 000 achievable from this model. A trade quota
   must not force entries.

### Out-of-sample scores (walk-forward, folds 1–4)

| model | target | status | n_oos | base rate | mean p (raw) | Brier raw | Brier clim | skill raw | skill recal | ECE raw | ECE recal | log loss raw |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `desk.fp.v1` | `tp_first_long` | calibrating | 3145 | 0.143 | 0.157 | 0.1290 | 0.1380 | 0.0652 | 0.0050 | 0.0573 | 0.0804 | 0.4401 |
| `desk.fp.v1` | `sl_first_long` | calibrating | 3145 | 0.279 | 0.340 | 0.2221 | 0.2033 | -0.0925 | -0.0132 | 0.0986 | 0.0406 | 0.6404 |
| `desk.fp.v1` | `tp_first_short` | calibrating | 3145 | 0.123 | 0.152 | 0.1170 | 0.1084 | -0.0795 | -0.0021 | 0.0698 | 0.0134 | 0.4057 |
| `desk.fp.v1` | `sl_first_short` | calibrating | 3145 | 0.311 | 0.348 | 0.2317 | 0.2348 | 0.0136 | -0.0087 | 0.0967 | 0.0928 | 0.6642 |
| `desk.fp.v1` | `up_1s` | calibrating | 1476 | 0.499 | 0.500 | 0.2491 | 0.2522 | 0.0124 | 0.0035 | 0.0186 | 0.0408 | 0.6914 |
| `desk.fp.v1` | `up_2s` | calibrating | 1835 | 0.503 | 0.501 | 0.2480 | 0.2524 | 0.0172 | 0.0085 | 0.0256 | 0.0508 | 0.6892 |
| `desk.fp.v1` | `up_5s` | calibrating | 2217 | 0.518 | 0.502 | 0.2479 | 0.2501 | 0.0087 | 0.0040 | 0.0167 | 0.0107 | 0.6889 |
| `desk.fp.v1` | `up_10s` | calibrating | 2531 | 0.500 | 0.502 | 0.2496 | 0.2505 | 0.0034 | 0.0004 | 0.0137 | 0.0183 | 0.6924 |
| `desk.fp.v1` | `up_15s` | calibrating | 2618 | 0.511 | 0.502 | 0.2517 | 0.2501 | -0.0062 | -0.0090 | 0.0313 | 0.0348 | 0.6967 |
| `desk.fp.v1` | `up_30s` | calibrating | 2737 | 0.512 | 0.503 | 0.2577 | 0.2515 | -0.0246 | -0.0033 | 0.0599 | 0.0408 | 0.7093 |
| `desk.fp.v1` | `up_60s` | calibrating | 2716 | 0.523 | 0.503 | 0.2622 | 0.2564 | -0.0229 | -0.0070 | 0.0760 | 0.0685 | 0.7207 |
| `desk.fp.v1` | `up_90s` | calibrating | 2802 | 0.519 | 0.505 | 0.2638 | 0.2610 | -0.0107 | -0.0150 | 0.0771 | 0.0892 | 0.7263 |
| `desk.fp.v1` | `up_120s` | calibrating | 2783 | 0.516 | 0.506 | 0.2668 | 0.2644 | -0.0091 | -0.0170 | 0.0905 | 0.0783 | 0.7354 |
| `mom.fp.v1` | `tp_first_long` | calibrating | 160590 | 0.057 | 0.143 | 0.0640 | 0.0557 | -0.1483 | 0.0019 | 0.0865 | 0.0382 | 0.2648 |
| `mom.fp.v1` | `sl_first_long` | calibrating | 160590 | 0.145 | 0.335 | 0.1682 | 0.1253 | -0.3417 | 0.0030 | 0.1948 | 0.0247 | 0.5237 |
| `mom.fp.v1` | `tp_first_short` | calibrating | 160590 | 0.050 | 0.141 | 0.0581 | 0.0485 | -0.1996 | -0.0015 | 0.0916 | 0.0287 | 0.2485 |
| `mom.fp.v1` | `sl_first_short` | calibrating | 160590 | 0.154 | 0.338 | 0.1722 | 0.1343 | -0.2824 | 0.0048 | 0.1903 | 0.0565 | 0.5324 |
| `mom.fp.v1` | `up_1s` | calibrating | 24485 | 0.500 | 0.500 | 0.2509 | 0.2501 | -0.0032 | 0.0067 | 0.0486 | 0.0067 | 0.6949 |
| `mom.fp.v1` | `up_2s` | calibrating | 34833 | 0.501 | 0.500 | 0.2513 | 0.2501 | -0.0048 | 0.0059 | 0.0513 | 0.0171 | 0.6957 |
| `mom.fp.v1` | `up_5s` | calibrating | 51794 | 0.505 | 0.501 | 0.2524 | 0.2504 | -0.0081 | 0.0066 | 0.0592 | 0.0030 | 0.6980 |
| `mom.fp.v1` | `up_10s` | calibrating | 72776 | 0.504 | 0.501 | 0.2538 | 0.2506 | -0.0127 | 0.0069 | 0.0676 | 0.0025 | 0.7008 |
| `mom.fp.v1` | `up_15s` | calibrating | 84046 | 0.508 | 0.501 | 0.2552 | 0.2513 | -0.0155 | 0.0066 | 0.0751 | 0.0108 | 0.7036 |
| `mom.fp.v1` | `up_30s` | calibrating | 102103 | 0.502 | 0.502 | 0.2581 | 0.2511 | -0.0280 | 0.0062 | 0.0872 | 0.0165 | 0.7096 |
| `mom.fp.v1` | `up_60s` | calibrating | 113814 | 0.506 | 0.501 | 0.2613 | 0.2514 | -0.0393 | 0.0098 | 0.0947 | 0.0191 | 0.7165 |
| `mom.fp.v1` | `up_90s` | calibrating | 121534 | 0.499 | 0.502 | 0.2643 | 0.2511 | -0.0525 | 0.0070 | 0.1045 | 0.0385 | 0.7231 |
| `mom.fp.v1` | `up_120s` | calibrating | 122635 | 0.495 | 0.502 | 0.2683 | 0.2512 | -0.0680 | 0.0046 | 0.1170 | 0.0494 | 0.7320 |
| `mom.fp.v1.1m` | `tp_first_long` | calibrating | 240425 | 0.119 | 0.145 | 0.1081 | 0.1047 | -0.0320 | 0.0020 | 0.0480 | 0.0086 | 0.3790 |
| `mom.fp.v1.1m` | `sl_first_long` | calibrating | 240425 | 0.255 | 0.339 | 0.2048 | 0.1901 | -0.0774 | 0.0052 | 0.1014 | 0.0070 | 0.6027 |
| `mom.fp.v1.1m` | `tp_first_short` | calibrating | 240425 | 0.114 | 0.145 | 0.1046 | 0.1011 | -0.0349 | 0.0035 | 0.0530 | 0.0020 | 0.3706 |
| `mom.fp.v1.1m` | `sl_first_short` | calibrating | 240425 | 0.261 | 0.338 | 0.2063 | 0.1927 | -0.0704 | 0.0037 | 0.0948 | 0.0116 | 0.6056 |
| `mom.fp.v1.1m` | `up_60s` | calibrating | 221140 | 0.496 | 0.500 | 0.2576 | 0.2500 | -0.0303 | -0.0001 | 0.0624 | 0.0091 | 0.7094 |
| `mom.fp.v1.1m` | `up_120s` | calibrating | 226973 | 0.496 | 0.500 | 0.2644 | 0.2500 | -0.0573 | 0.0002 | 0.0901 | 0.0052 | 0.7254 |

Recording: 40.0 min, 2032 RT polls, 505 futures pairs, 5916 depth-20 books on 10 pairs, fees from metadata: {'B-BNB_USDT': 0.0005899999999999999, 'B-BTC_USDT': 0.0005899999999999999, 'B-ETH_USDT': 0.0005899999999999999, 'B-PAXG_USDT': 0.000118, 'B-SOL_USDT': 0.0005899999999999999, 'B-ZEC_USDT': 0.0005899999999999999}

**desk.fp.v1** — 3935 decisions on 10 pairs, stride 5.0 s (~164 non-overlapping 120 s windows), up-label tie fraction 0.165

With measured cost: n=3935, mean round-trip cost 13.40 bps, mean TP 12.39 bps / SL 8.26 bps.
- every long: gross 0.07 bps, net -13.33 bps; TP-first realised 0.165 vs predicted 0.158; SL-first realised 0.287
- every short: gross -0.14 bps, net -13.54 bps; TP-first realised 0.135 vs predicted 0.149; SL-first realised 0.328

| cost × | EV>0 trades | mean predicted EV bps | mean realised net bps | win rate | sum net bps | max DD bps |
|---:|---:|---:|---:|---:|---:|---:|
| 0.0 | 3834 | 2.10 | 0.18 | 0.497 | 690.5 | 1312.4 |
| 0.25 | 762 | 2.07 | -3.50 | 0.374 | -2666.4 | 2712.3 |
| 0.5 | 173 | 2.18 | -7.07 | 0.335 | -1223.6 | 1223.6 |
| 1.0 | 4 | 0.56 | -8.93 | 0.000 | -35.7 | 35.7 |
| 1.5 | 0 | — | — | — | -0.0 | 0.0 |
| 2.0 | 0 | — | — | — | -0.0 | 0.0 |

**mom.fp.v1** — 199687 decisions on 505 pairs, stride 5.0 s (~8320 non-overlapping 120 s windows), up-label tie fraction 0.449

With measured cost: n=3907, mean round-trip cost 13.40 bps, mean TP 12.38 bps / SL 8.26 bps.
- every long: gross 0.08 bps, net -13.33 bps; TP-first realised 0.166 vs predicted 0.151; SL-first realised 0.287
- every short: gross -0.13 bps, net -13.54 bps; TP-first realised 0.136 vs predicted 0.141; SL-first realised 0.329

| cost × | EV>0 trades | mean predicted EV bps | mean realised net bps | win rate | sum net bps | max DD bps |
|---:|---:|---:|---:|---:|---:|---:|
| 0.0 | 3641 | 1.58 | 0.23 | 0.496 | 853.5 | 1422.0 |
| 0.25 | 428 | 1.56 | -3.04 | 0.376 | -1301.3 | 1386.2 |
| 0.5 | 56 | 1.54 | -9.30 | 0.232 | -520.6 | 600.6 |
| 1.0 | 0 | — | — | — | -0.0 | 0.0 |
| 1.5 | 0 | — | — | — | -0.0 | 0.0 |
| 2.0 | 0 | — | — | — | -0.0 | 0.0 |

**mom.fp.v1.1m** — 300512 decisions on 30 pairs, stride 60.0 s (~150256 non-overlapping 120 s windows), up-label tie fraction 0.065

With measured cost: n=100171, mean round-trip cost 13.41 bps, mean TP 17.75 bps / SL 11.83 bps.
- every long: gross 0.03 bps, net -13.38 bps; TP-first realised 0.120 vs predicted 0.145; SL-first realised 0.254
- every short: gross 0.25 bps, net -13.16 bps; TP-first realised 0.115 vs predicted 0.146; SL-first realised 0.257

| cost × | EV>0 trades | mean predicted EV bps | mean realised net bps | win rate | sum net bps | max DD bps |
|---:|---:|---:|---:|---:|---:|---:|
| 0.0 | 91203 | 2.15 | 0.26 | 0.468 | 23472.6 | 4821.7 |
| 0.25 | 18090 | 2.64 | -2.91 | 0.402 | -52732.2 | 52738.8 |
| 0.5 | 4990 | 3.31 | -5.83 | 0.373 | -29075.2 | 29102.6 |
| 1.0 | 762 | 4.53 | -13.59 | 0.323 | -10353.5 | 10464.7 |
| 1.5 | 181 | 5.66 | -18.61 | 0.298 | -3369.0 | 3383.7 |
| 2.0 | 58 | 6.92 | -24.10 | 0.362 | -1398.0 | 1398.0 |


## Not done / next
* Trade-tape features (aggressor flow and intensity) are recorded but not yet scored.
  The WebSocket feed (s2-ws) will replace 1 s RT polling for barrier monitoring.
* No parameter search was run. The desk priors are scored as-is. Any (λ, γ, k_s, k_t)
  search must use the same walk-forward split, so the search itself isn't fitted in-sample.
* XAU instrument metadata failed to decode (qe-coindcx type), so the default 0.059 % taker fee was used for it. PAXG metadata reports 0.0118 %.
* Book-dependent results cover about 40 minutes of 10 pairs, which is far too short to
  claim calibration of `desk.fp.v1`. Keep recording.
