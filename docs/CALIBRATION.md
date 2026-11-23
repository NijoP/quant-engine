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

The committed `crates/qe-research/calibration.json` was regenerated in sprint 3 from the 30.6-minute recording of 18:50–19:21 IST and 5 days of 1m history on 20 pairs (see the sprint 3 section). The tables below are the sprint 2 run.
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


## Sprint 3: champion/challenger results (2026-11-23, 18:50–19:21 IST)

Reproduce with `qe-research record --out REC --minutes 34 --focus 8`, then
`qe-research challenge --rec REC --out crates/qe-research/models.json --stride 2`.
s3-learn and s3-desk load the registry through `qe_research::registry::ModelRegistry`.
A missing file means the champion only and nothing promotable.

**Headline (honest):**
1. **No challenger has an edge after costs, and none is promotable.** Every model loses
   money out of sample in every cost scenario, including the optimistic maker/maker bound
   (2 × 0.0236 % fee, no spread, perfect passive fills).
   * The nested parameter search over k_s, k_t and the EV threshold took no trade in any
     fold: the best past cell never had positive net. It abstained, as designed.
   * The fixed champion bracket at EV > 0 realised −9.5 to −13.1 bps per trade with
     taker costs. Only 0–12 trades passed the filter, too few to measure anything except
     the sign.
2. **The order-flow logistic (`logit.flow.v2`) is worse than climatology** on every target
   (Brier skill −0.07 to −0.62). It overfits on about 6-minute folds whose regime keeps
   shifting.
3. **The conditional-historical cells (`cond.flow.v2`) come closest:** Brier skill +0.009
   to +0.011 on three of the four bracket targets. That is still below the 0.01 gate on
   most targets and not consistent across folds (2–3 of 4). The ECE of 0.10–0.22 on the SL
   targets fails, so it is not calibrated.
4. **Isotonic and Platt recalibration of the champion bring no skill on this sample**
   (skill −0.002 to −0.015). The training-fold base rate does not carry over: the session
   drifted down, so SL-first for longs was realised at 0.39 against 0.19 for shorts.
5. **The champion's large positive "skill" on some targets is an artifact.** It shows up
   on `sl_first_long`, `tp_first_short` and `up_120s` (+0.17 to +0.22) only because the
   climatology reference comes from earlier folds whose base rate differed. Its ECE is
   0.05–0.14, it is positive in only 1–3 of 4 folds, and the same sample loses money.
   It is not evidence of edge.
6. **The champion on the larger datasets (`evaluate`, refreshed below) stays `calibrating`
   on all 32 entries.** On 5 days × 20 pairs of 1m history, EV>0 selections at 1× cost
   realise −13.05 bps per trade against +4.58 bps predicted. The forecast still overstates
   EV and costs about 13 bps per round trip.
7. Consequence: **the registry champion stays `desk.fp.v1`, with rollback `desk.fp.v1`
   and nothing promotable.** The desk must keep withholding strategy authority. The target
   controller must not treat any forecast here as an edge.

### Promotion criteria (fixed before the run, also stored in `models.json`)
* calibration, each of tp_first_{long,short} and sl_first_{long,short} at the champion bracket (k_s=1.0, k_t=1.5): n_oos >= 1000 over >= 3 OOS folds
* calibration: ECE (10 bins) <= 0.03 and Brier skill vs training-fold climatology >= 0.01, skill > 0 in >= 2/3 of OOS folds
* trading under the taker_taker cost (2 x taker fee + spread + 2 x 1 bp), one bracket per pair at a time: n_trades >= 100
* trading: mean realised net > 0 bps with t-stat >= 2, and positive net in >= 2/3 of >= 3 OOS folds
* challengers with a parameter search are judged on the nested walk-forward result (config for fold k chosen on folds < k), never on the best grid cell
* maker_taker and maker_maker results are reported but cannot promote: passive fills and adverse selection are not modelled

### Results

Data: 30.6 min live recording, 1554 RT polls, 4583 valid depth-20 books on 8 pairs, 22091 de-duplicated public trades; 4936 decision rows every 2.0 s (~82 non-overlapping 120 s windows); aggressor-sign agreement with the tick rule 0.933.

#### Probability scores (OOS folds 1–4, champion bracket k_s = 1.0, k_t = 1.5)

| model | target | n_oos | base rate | mean p | Brier | Brier clim | skill | ECE | log loss | folds skill>0 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `desk.fp.v1` | `sl_first_long` | 3928 | 0.392 | 0.359 | 0.2533 | 0.3210 | 0.2107 | 0.103 | 0.7092 | 1/4 |
| `desk.fp.v1` | `sl_first_short` | 3928 | 0.191 | 0.327 | 0.1856 | 0.1682 | -0.1037 | 0.138 | 0.5567 | 1/4 |
| `desk.fp.v1` | `tp_first_long` | 3928 | 0.090 | 0.142 | 0.0896 | 0.0814 | -0.1013 | 0.053 | 0.3246 | 1/4 |
| `desk.fp.v1` | `tp_first_short` | 3928 | 0.226 | 0.164 | 0.1826 | 0.2201 | 0.1702 | 0.075 | 0.5767 | 3/4 |
| `desk.fp.v1` | `up_10s` | 3288 | 0.454 | 0.495 | 0.2508 | 0.2514 | 0.0022 | 0.041 | 0.6948 | 2/4 |
| `desk.fp.v1` | `up_120s` | 3548 | 0.365 | 0.482 | 0.2657 | 0.3354 | 0.2080 | 0.143 | 0.7301 | 2/4 |
| `desk.fp.v1` | `up_30s` | 3517 | 0.441 | 0.490 | 0.2593 | 0.2626 | 0.0123 | 0.080 | 0.7125 | 2/4 |
| `desk.fp.v1` | `up_60s` | 3562 | 0.399 | 0.486 | 0.2628 | 0.2868 | 0.0837 | 0.116 | 0.7208 | 2/4 |
| `desk.fp.v1+iso` | `sl_first_long` | 3928 | 0.392 | 0.244 | 0.3224 | 0.3210 | -0.0044 | 0.214 | 1.0471 | 1/4 |
| `desk.fp.v1+iso` | `sl_first_short` | 3928 | 0.191 | 0.283 | 0.1705 | 0.1682 | -0.0140 | 0.095 | 0.5268 | 1/4 |
| `desk.fp.v1+iso` | `tp_first_long` | 3928 | 0.090 | 0.106 | 0.0818 | 0.0814 | -0.0052 | 0.016 | 0.3001 | 1/4 |
| `desk.fp.v1+iso` | `tp_first_short` | 3928 | 0.226 | 0.154 | 0.2234 | 0.2201 | -0.0149 | 0.185 | 1.3271 | 2/4 |
| `desk.fp.v1+iso` | `up_10s` | 3288 | 0.454 | 0.500 | 0.2524 | 0.2514 | -0.0043 | 0.060 | 0.6981 | 2/4 |
| `desk.fp.v1+iso` | `up_120s` | 3548 | 0.365 | 0.610 | 0.3391 | 0.3354 | -0.0110 | 0.245 | 0.9049 | 2/4 |
| `desk.fp.v1+iso` | `up_30s` | 3517 | 0.441 | 0.517 | 0.2662 | 0.2626 | -0.0138 | 0.152 | 0.7260 | 1/4 |
| `desk.fp.v1+iso` | `up_60s` | 3562 | 0.399 | 0.548 | 0.2888 | 0.2868 | -0.0070 | 0.185 | 0.7741 | 2/4 |
| `desk.fp.v1+platt` | `sl_first_long` | 3928 | 0.392 | 0.242 | 0.3224 | 0.3210 | -0.0045 | 0.257 | 1.0161 | 2/4 |
| `desk.fp.v1+platt` | `sl_first_short` | 3928 | 0.191 | 0.285 | 0.1704 | 0.1682 | -0.0134 | 0.093 | 0.5265 | 1/4 |
| `desk.fp.v1+platt` | `tp_first_long` | 3928 | 0.090 | 0.106 | 0.0815 | 0.0814 | -0.0020 | 0.016 | 0.2975 | 1/4 |
| `desk.fp.v1+platt` | `tp_first_short` | 3928 | 0.226 | 0.153 | 0.2221 | 0.2201 | -0.0092 | 0.186 | 1.1478 | 1/4 |
| `desk.fp.v1+platt` | `up_10s` | 3288 | 0.454 | 0.502 | 0.2544 | 0.2514 | -0.0121 | 0.052 | 0.7020 | 1/4 |
| `desk.fp.v1+platt` | `up_120s` | 3548 | 0.365 | 0.601 | 0.3396 | 0.3354 | -0.0124 | 0.238 | 0.9010 | 2/4 |
| `desk.fp.v1+platt` | `up_30s` | 3517 | 0.441 | 0.522 | 0.2676 | 0.2626 | -0.0193 | 0.137 | 0.7296 | 1/4 |
| `desk.fp.v1+platt` | `up_60s` | 3562 | 0.399 | 0.547 | 0.2919 | 0.2868 | -0.0179 | 0.209 | 0.7812 | 1/4 |
| `logit.flow.v2` | `sl_first_long` | 3928 | 0.392 | 0.263 | 0.3448 | 0.3210 | -0.0742 | 0.262 | 2.0052 | 1/4 |
| `logit.flow.v2` | `sl_first_short` | 3928 | 0.191 | 0.420 | 0.2732 | 0.1682 | -0.6245 | 0.228 | 0.8098 | 1/4 |
| `logit.flow.v2` | `tp_first_long` | 3928 | 0.090 | 0.177 | 0.1157 | 0.0814 | -0.4223 | 0.087 | 0.3822 | 2/4 |
| `logit.flow.v2` | `tp_first_short` | 3928 | 0.226 | 0.194 | 0.2403 | 0.2201 | -0.0918 | 0.231 | 1.4860 | 1/4 |
| `logit.flow.v2` | `up_10s` | 3288 | 0.454 | 0.569 | 0.2879 | 0.2514 | -0.1455 | 0.130 | 0.8050 | 0/4 |
| `logit.flow.v2` | `up_120s` | 3548 | 0.365 | 0.623 | 0.3958 | 0.3354 | -0.1799 | 0.264 | 1.5237 | 1/4 |
| `logit.flow.v2` | `up_30s` | 3517 | 0.441 | 0.609 | 0.3333 | 0.2626 | -0.2694 | 0.209 | 1.0206 | 0/4 |
| `logit.flow.v2` | `up_60s` | 3562 | 0.399 | 0.631 | 0.3734 | 0.2868 | -0.3018 | 0.251 | 1.3315 | 0/4 |
| `cond.flow.v2` | `sl_first_long` | 3928 | 0.392 | 0.244 | 0.3180 | 0.3210 | 0.0092 | 0.221 | 1.0445 | 2/4 |
| `cond.flow.v2` | `sl_first_short` | 3928 | 0.191 | 0.287 | 0.1665 | 0.1682 | 0.0099 | 0.096 | 0.5152 | 3/4 |
| `cond.flow.v2` | `tp_first_long` | 3928 | 0.090 | 0.107 | 0.0805 | 0.0814 | 0.0106 | 0.016 | 0.2907 | 2/4 |
| `cond.flow.v2` | `tp_first_short` | 3928 | 0.226 | 0.154 | 0.2209 | 0.2201 | -0.0037 | 0.191 | 1.2301 | 2/4 |
| `cond.flow.v2` | `up_10s` | 3288 | 0.454 | 0.500 | 0.2526 | 0.2514 | -0.0049 | 0.058 | 0.6985 | 2/4 |
| `cond.flow.v2` | `up_120s` | 3548 | 0.365 | 0.602 | 0.3295 | 0.3354 | 0.0178 | 0.238 | 0.8957 | 3/4 |
| `cond.flow.v2` | `up_30s` | 3517 | 0.441 | 0.519 | 0.2661 | 0.2626 | -0.0133 | 0.101 | 0.7272 | 1/4 |
| `cond.flow.v2` | `up_60s` | 3562 | 0.399 | 0.549 | 0.2904 | 0.2868 | -0.0125 | 0.183 | 0.7859 | 1/4 |

#### Trading after costs (bps of notional per trade, one bracket per pair at a time)

| model | scenario | basis | trades | mean net | t-stat | win rate | sum net | folds + |
|---|---|---|---:|---:|---:|---:|---:|---:|
| `desk.fp.v1` | maker_maker | fixed k_s=1, k_t=1.5, EV>0 | 41 | -3.81 | -2.20 | 0.390 | -156.3 | 1/4 |
| `desk.fp.v1` | maker_taker | fixed k_s=1, k_t=1.5, EV>0 | 8 | -5.18 | -1.25 | 0.125 | -41.4 | 1/3 |
| `desk.fp.v1` | taker_taker | fixed k_s=1, k_t=1.5, EV>0 | 1 | -9.57 | — | 0.000 | -9.6 | 0/1 |
| `desk.fp.v1+iso` | maker_maker | fixed k_s=1, k_t=1.5, EV>0 | 28 | -9.53 | -5.88 | 0.143 | -266.9 | 2/4 |
| `desk.fp.v1+iso` | maker_taker | fixed k_s=1, k_t=1.5, EV>0 | 8 | -9.85 | -2.30 | 0.250 | -78.8 | 0/2 |
| `desk.fp.v1+iso` | taker_taker | fixed k_s=1, k_t=1.5, EV>0 | 2 | -9.48 | -3.72 | 0.000 | -19.0 | 0/1 |
| `desk.fp.v1+platt` | maker_maker | fixed k_s=1, k_t=1.5, EV>0 | 13 | -13.04 | -5.00 | 0.077 | -169.5 | 0/3 |
| `desk.fp.v1+platt` | maker_taker | fixed k_s=1, k_t=1.5, EV>0 | 3 | -8.41 | -4.60 | 0.000 | -25.2 | 0/1 |
| `desk.fp.v1+platt` | taker_taker | fixed k_s=1, k_t=1.5, EV>0 | 0 | — | — | — | -0.0 | 0/0 |
| `logit.flow.v2` | maker_maker | fixed k_s=1, k_t=1.5, EV>0 | 46 | -6.96 | -4.53 | 0.217 | -320.0 | 1/4 |
| `logit.flow.v2` | maker_maker | nested search | 0 | — | — | — | -0.0 | 0/0 |
| `logit.flow.v2` | maker_taker | fixed k_s=1, k_t=1.5, EV>0 | 19 | -10.27 | -3.44 | 0.211 | -195.1 | 0/3 |
| `logit.flow.v2` | maker_taker | nested search | 0 | — | — | — | -0.0 | 0/0 |
| `logit.flow.v2` | taker_taker | fixed k_s=1, k_t=1.5, EV>0 | 12 | -13.12 | -3.08 | 0.250 | -157.5 | 0/3 |
| `logit.flow.v2` | taker_taker | nested search | 0 | — | — | — | -0.0 | 0/0 |
| `cond.flow.v2` | maker_maker | fixed k_s=1, k_t=1.5, EV>0 | 16 | -10.26 | -4.88 | 0.062 | -164.1 | 0/4 |
| `cond.flow.v2` | maker_maker | nested search | 0 | — | — | — | -0.0 | 0/0 |
| `cond.flow.v2` | maker_taker | fixed k_s=1, k_t=1.5, EV>0 | 3 | -8.94 | -5.11 | 0.000 | -26.8 | 0/1 |
| `cond.flow.v2` | maker_taker | nested search | 0 | — | — | — | -0.0 | 0/0 |
| `cond.flow.v2` | taker_taker | fixed k_s=1, k_t=1.5, EV>0 | 2 | -11.82 | -132.74 | 0.000 | -23.6 | 0/1 |
| `cond.flow.v2` | taker_taker | nested search | 0 | — | — | — | -0.0 | 0/0 |

#### Best grid cells (selection-biased: shown only to bound what a search could find)

| model | scenario | k_s | k_t | thr bps | trades | mean net | t-stat |
|---|---|---:|---:|---:|---:|---:|---:|
| `logit.flow.v2` | maker_taker | 0.5 | 1.5 | 0.0 | 20 | -9.83 | -5.93 |
| `logit.flow.v2` | maker_maker | 0.5 | 1.5 | 0.0 | 45 | -5.34 | -4.30 |

#### Promotion decision

Champion: `desk.fp.v1` (no challenger passed the predefined out-of-sample gates; the champion stays (it has no strategy authority unless qe-research calibration grants it)). Rollback: `desk.fp.v1`.

* `desk.fp.v1` (champion): **not promotable**: 13 gate failures, e.g. tp_first_long: ECE 0.053 > 0.03; tp_first_long: Brier skill -0.1013 < 0.01; tp_first_long: skill > 0 in 1/4 folds
* `desk.fp.v1+iso` (challenger): **not promotable**: 15 gate failures, e.g. tp_first_long: Brier skill -0.0052 < 0.01; tp_first_long: skill > 0 in 1/4 folds; sl_first_long: ECE 0.214 > 0.03
* `desk.fp.v1+platt` (challenger): **not promotable**: 15 gate failures, e.g. tp_first_long: Brier skill -0.0020 < 0.01; tp_first_long: skill > 0 in 1/4 folds; sl_first_long: ECE 0.257 > 0.03
* `logit.flow.v2` (challenger): **not promotable**: 16 gate failures, e.g. tp_first_long: ECE 0.087 > 0.03; tp_first_long: Brier skill -0.4223 < 0.01; tp_first_long: skill > 0 in 2/4 folds
* `cond.flow.v2` (challenger): **not promotable**: 13 gate failures, e.g. tp_first_long: skill > 0 in 2/4 folds; sl_first_long: ECE 0.221 > 0.03; sl_first_long: Brier skill 0.0092 < 0.01

#### Champion on the larger datasets (`qe-research evaluate`, refreshed)

| model | target | n_oos | skill raw | skill recal | ECE raw | ECE recal | status |
|---|---|---:|---:|---:|---:|---:|---|
| `desk.fp.v1` | `tp_first_long` | 1888 | -0.1041 | -0.0060 | 0.055 | 0.018 | calibrating |
| `desk.fp.v1` | `sl_first_long` | 1888 | 0.2199 | -0.0111 | 0.092 | 0.221 | calibrating |
| `desk.fp.v1` | `tp_first_short` | 1888 | 0.1767 | -0.0097 | 0.072 | 0.186 | calibrating |
| `desk.fp.v1` | `sl_first_short` | 1888 | -0.0969 | -0.0207 | 0.134 | 0.124 | calibrating |
| `desk.fp.v1` | `up_60s` | 1716 | 0.0845 | -0.0251 | 0.100 | 0.185 | calibrating |
| `desk.fp.v1` | `up_120s` | 1735 | 0.2250 | -0.0335 | 0.145 | 0.273 | calibrating |
| `mom.fp.v1` | `tp_first_long` | 119180 | -0.2582 | -0.0105 | 0.103 | 0.061 | calibrating |
| `mom.fp.v1` | `sl_first_long` | 119180 | -0.0126 | -0.0005 | 0.124 | 0.150 | calibrating |
| `mom.fp.v1` | `tp_first_short` | 119180 | 0.0271 | 0.0027 | 0.059 | 0.065 | calibrating |
| `mom.fp.v1` | `sl_first_short` | 119180 | -0.4466 | -0.0130 | 0.222 | 0.100 | calibrating |
| `mom.fp.v1` | `up_60s` | 89649 | 0.0245 | -0.0030 | 0.107 | 0.145 | calibrating |
| `mom.fp.v1` | `up_120s` | 96408 | 0.0513 | -0.0141 | 0.115 | 0.155 | calibrating |
| `mom.fp.v1.1m` | `tp_first_long` | 114182 | -0.0375 | 0.0015 | 0.051 | 0.004 | calibrating |
| `mom.fp.v1.1m` | `sl_first_long` | 114182 | -0.0756 | 0.0060 | 0.104 | 0.003 | calibrating |
| `mom.fp.v1.1m` | `tp_first_short` | 114182 | -0.0286 | 0.0045 | 0.051 | 0.007 | calibrating |
| `mom.fp.v1.1m` | `sl_first_short` | 114182 | -0.0807 | 0.0042 | 0.103 | 0.009 | calibrating |
| `mom.fp.v1.1m` | `up_60s` | 104984 | -0.0320 | 0.0001 | 0.065 | 0.009 | calibrating |
| `mom.fp.v1.1m` | `up_120s` | 107749 | -0.0603 | 0.0005 | 0.093 | 0.011 | calibrating |

* `desk.fp.v1`: mean cost 13.14 bps; unconditional long gross -0.79 bps, short gross 0.81 bps; EV>0 at 1× cost: 15 trades, mean realised net -3.62 bps vs predicted 0.71 bps.
* `mom.fp.v1`: mean cost 13.14 bps; unconditional long gross -0.79 bps, short gross 0.81 bps; EV>0 at 1× cost: 0 trades, mean realised net — bps vs predicted — bps.
* `mom.fp.v1.1m`: mean cost 13.14 bps; unconditional long gross 0.01 bps, short gross 0.26 bps; EV>0 at 1× cost: 539 trades, mean realised net -13.05 bps vs predicted 4.58 bps.


## Sprint 5: recalibration, cost-aware brackets and a liquid universe (2026-11-23)

The question was whether an honest change to the TP-first/SL-first forecast can earn strategy
authority without loosening any gate. Short answer: **no candidate did. The gates and
thresholds are unchanged, and the champion and registry rules still deny authority.**

New challengers in `qe-research challenge`. They are scored with the same purged
chronological walk-forward, the same gates and the same nested parameter search:
* `desk.fp.v1+platt.wide`: Platt-recalibrated champion probabilities, with the bracket
  searched over a wider, cost-aware grid (k_s ∈ {1, 1.5, 2, 3}, k_t ∈ {1.5, 2, 3, 4}, so TP can
  reach 4·σ₁₂₀ and is not smaller than the ~13 bps cost) plus the EV threshold. Judged on the
  nested result only.
* `desk.fp.v1+platt.liquid`, `logit.flow.v2.liquid`: the same models restricted to the
  cheapest half of the pairs, ranked by **fold-0** median round-trip cost (fold 0 is never
  scored, so choosing the universe sees no OOS data; unit-tested).
* Isotonic and Platt recalibration fitted on past folds already existed
  (`desk.fp.v1+iso`, `+platt`). They are the offline equivalent of recalibrating the live
  forward monitor: each forecast is mapped using only outcomes resolved before it.

Data (public endpoints only):
* **R1** is the sprint-3 recording: 34.0 min, 8 book pairs, 5 514 rows, about 92
  independent 120 s windows.
* **R2** is new: 44.8 min (16:20–17:05 UTC, 21:50–22:35 IST), 10 book pairs, 9 248 rows,
  about 154 windows, recorded through the new public client with 0 breaker opens and 4
  transport errors.

`crates/qe-research/models.json` is regenerated from R2.

### Out-of-sample calibration, P(TP first) at the champion bracket (folds 1–4)

| model | data | target | n_oos | base | mean p | Brier | Brier clim | skill | ECE |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|
| `desk.fp.v1` (raw) | R1 | long | 4386 | 0.081 | 0.134 | 0.0801 | 0.0778 | −0.029 | 0.054 |
| `desk.fp.v1` (raw) | R1 | short | 4386 | 0.228 | 0.172 | 0.1855 | 0.2210 | +0.161 | 0.076 |
| `desk.fp.v1+platt` | R1 | long | 4386 | 0.081 | 0.122 | 0.0771 | 0.0778 | +0.009 | 0.041 |
| `desk.fp.v1+platt` | R1 | short | 4386 | 0.228 | 0.154 | 0.2228 | 0.2210 | −0.008 | 0.204 |
| `desk.fp.v1+platt.liquid` | R1 | long | 2192 | 0.097 | 0.145 | 0.0929 | 0.0949 | +0.021 | 0.054 |
| `desk.fp.v1+platt.liquid` | R1 | short | 2192 | 0.271 | 0.171 | 0.2548 | 0.2502 | −0.018 | 0.234 |
| `desk.fp.v1` (raw) | R2 | long | 7430 | 0.094 | 0.145 | 0.0940 | 0.0875 | −0.074 | 0.067 |
| `desk.fp.v1` (raw) | R2 | short | 7430 | 0.216 | 0.164 | 0.1728 | 0.2033 | +0.150 | 0.058 |
| `desk.fp.v1+iso` | R2 | long | 7430 | 0.094 | 0.052 | 0.0878 | 0.0875 | −0.003 | 0.042 |
| `desk.fp.v1+platt` | R2 | long | 7430 | 0.094 | 0.053 | 0.0881 | 0.0875 | −0.007 | 0.042 |
| `desk.fp.v1+platt` | R2 | short | 7430 | 0.216 | 0.133 | 0.2042 | 0.2033 | −0.005 | 0.178 |
| `logit.flow.v2` | R2 | short | 7430 | 0.216 | 0.142 | 0.1945 | 0.2033 | +0.044 | 0.131 |
| `desk.fp.v1+platt.liquid` | R2 | long | 3715 | 0.113 | 0.057 | 0.1038 | 0.1026 | −0.011 | 0.058 |
| `logit.flow.v2.liquid` | R2 | short | 3715 | 0.202 | 0.113 | 0.1773 | 0.1915 | +0.074 | 0.129 |

`desk.fp.v1+platt.wide` is identical to `+platt` at the champion bracket, by construction.
The full tables for all four bracket targets and both recordings are in the registry.

### Trading, taker/taker cost (the only scenario that can promote), judged result

| model | R1 n / mean net bps | R2 n / mean net bps | R2 maker/maker bound |
|---|---|---|---|
| `desk.fp.v1` | 1 / −10.0 | 29 / −24.5 (t −6.2, 0/4 folds +) | −3.5 |
| `desk.fp.v1+iso` | 3 / −11.6 | 9 / −29.1 | −4.8 |
| `desk.fp.v1+platt` | 3 / −11.6 | 1 / +9.2 | −3.1 |
| `desk.fp.v1+platt.wide` (nested) | 0: abstained, no past cell ≥ 10 trades | 0: abstained, best past cell net ≤ 0 | −6.1 |
| `desk.fp.v1+platt.liquid` | 4 / −9.6 | 1 / −15.6 | −5.4 |
| `logit.flow.v2(.liquid)`, `cond.flow.v2` | 0 | 0 | −7.2 / +0.1 / −11.2 |

Per-cell (not nested, so optimistic) results of the wide grid at threshold 0: every cell
with ≥ 2 trades is negative on R1 (best −7.4 bps, k_s 1, k_t 3). On R2 the best cell
with ≥ 5 trades is −5.4 bps (n 24).

**Findings (negative results included):**
1. **Recalibration fixes the mean but not the skill.** Platt and isotonic bring the
   long-side mean p from 0.145 to 0.053 against a base rate of 0.094. Brier skill is
   still −0.003 to −0.007 on R2 and +0.009 on R1, below the 0.01 gate every time. ECE stays
   0.04–0.20, because the base rate moves between folds: SL-first long is 0.25 in training
   and 0.42 OOS on R1. A map fitted on the past is mis-scaled for the next 7–9 minutes.
2. **Raw short-side TP skill (+0.15 to +0.16 on both recordings) is not usable.** It
   comes with ECE 0.06–0.08, and the same model has negative skill on the mirror
   long/SL targets. Recalibrating it destroys the skill (−0.005 to −0.008). That is the
   signature of a base-rate coincidence, not information.
3. **Wider, cost-aware brackets do not create an edge.** No grid cell is reliably positive
   after the ~13 bps round-trip, and the nested search correctly abstains. Even the
   maker/maker upper bound, which has no spread and perfect passive fills, is negative for
   every searched model except `logit.flow.v2.liquid` on R2 (+0.1 bps).
4. **Fewer, more liquid, cheaper instruments help calibration a little on R1 (long skill
   +0.021) but not on R2 (−0.011).** The low-fee contracts (PAXG, XAU, XAG, CL at 0.0118 %
   taker) dominate the liquid half, yet the trades they select still lose.
5. Consequence: nothing is promotable, and the desk's calibration gate correctly keeps
   blocking entries. More forecasting variants on half-hour recordings are unlikely to
   change this. The data needed is hours of WS-fed recording with a paper-forward test, not
   more models.

Reproduce: `qe-research record --out R --minutes 45 --focus 10`, then
`qe-research challenge --rec R --out models.json --stride 2`.

## Public REST throttling (sprint 5)

The 38 % "circuit open" rejections reported for the 30-minute paper run at 64cdd3c
had this cause. Null-leverage instrument records failed to decode, the decode errors counted
toward the breaker, five in a row opened it, every metadata call for 30 s then failed fast,
and the desk stored each fast-fail as a per-instrument metadata failure. That rejected the
pair for 10 minutes. PR #14 fixed the decoding. This sprint removes the remaining paths:
* Decode errors and HTTP 4xx other than 429 no longer count toward the breaker. Only 429,
  timeouts, connection errors and 5xx do.
* After a 429, the client's request spacing doubles (125 ms → up to 2 s) and recovers by
  5 ms per success. The cooldown honours `Retry-After` and doubles (to 5 min) on a re-open
  within 5 min. All public clients in one process share a 12 req/s ceiling, below the
  documented 16/s.
* The desk's metadata loop treats a circuit-open or 429 as "wait out the cooldown". It
  keeps the cached record and records no instrument failure. Metadata stays cached, with a
  10-minute refresh for the top candidates.
* Every breaker transition becomes an incident: `rest_breaker_open` and `rest_breaker_close`
  in `incidents.jsonl` and the audit log, with the client name, reason, cooldown, time open
  and calls refused.

## Not done / next
* Trade-tape features are now scored (sprint 3). The tape is still REST-polled, so it can
  miss trades on busy pairs.
  The WebSocket feed (s2-ws) will replace 1 s RT polling for barrier monitoring.
* The k_s, k_t and threshold search now runs nested inside the walk-forward (sprint 3).
  λ and γ are still not searched.
* About 30 minutes on 8 pairs (~82 independent 120 s windows) cannot establish calibration.
  Recording longer, from the WS feed, and running a paper-forward test are prerequisites
  for any promotion.
* XAU instrument metadata failed to decode (qe-coindcx type), so the default 0.059 % taker fee was used for it. PAXG metadata reports 0.0118 %.
* Book-dependent results cover about 40 minutes of 10 pairs, which is far too short to
  claim calibration of `desk.fp.v1`. Keep recording.
