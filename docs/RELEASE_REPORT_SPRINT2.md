# Sprint 2 release report (s2-desk) — 23 Nov 2026

> **Historical report.** Figures are as recorded on 23 Nov 2026. The INR 18,000 target below is
> superseded by the 24 Nov campaign (INR 100 → INR 50,000 by 22:00 IST, [CAMPAIGN_24NOV.md](CAMPAIGN_24NOV.md)).
> Re-checked at main `22fabe5`: live execution is still not validated and no model is calibrated.

**Final decision: PAPER TRADING READY (pipeline) — no paper trade was taken, because no forecast has
authority and no candidate has positive expected net edge after costs. LIVE EXECUTION: NOT READY,
refused by construction.**

Branch `fm/s2-desk` (integrates `fm/s2-live` #5, `fm/s2-ws` #8, `fm/s2-calib` #7). All numbers below
come from runs of the `qe-desk` binary against live CoinDCX public data. Paper equity is a configured
INR 100, not a CoinDCX balance. No authenticated endpoint was called and no credential was read.

## Readiness (each from an observed runtime condition)

| Flag | Value | Evidence |
|---|---|---|
| COINDCX_PUBLIC_DATA_READY | YES | universe 981 active spot, 505 USDT + 505 INR futures; RT/WS prices fresh |
| ALL_INSTRUMENT_SCANNER_READY | YES | 505/505 futures ranked every second |
| MULTI_TIMEFRAME_ENGINE_READY | YES | 7/7 contexts (3d, 2d derived, 1d, 1h, 15m, 5m, 1m) for each candidate |
| FORECAST_ENGINE_READY | YES | 1–120 s horizons + first-passage for every candidate with a sane book |
| FORECAST_CALIBRATED | **NO** | s2-calib registry: `desk.fp.v1` *calibrating* (ECE 0.080 > 0.03, Brier skill 0.005 < 0.01); forward monitor also fails (below) |
| MATHEMATICAL_DECISION_ENGINE_READY | YES | every candidate gets a decision with a full numeric explanation (decisions.jsonl) |
| RISK_GOVERNOR_READY | YES | every strategy proposal goes through `RiskGovernor::evaluate_proposal`; rejections logged |
| PAPER_EXECUTION_READY | YES (unit-tested; not exercised live) | lifecycle wired to an independent 250 ms timer; desk tests cover fill → protect → SL exit, 117.5 s time stop, flatten |
| AUTHENTICATED_ACCOUNT_READY | NO | the desk never reads credentials; owner runs `live_readiness` |
| LIVE_ORDER_ADAPTER_READY | NO | s2-live gate: feature not compiled, mode paper, checklist items not run |
| LIVE_EXECUTION_VALIDATED | NO | never attempted |

## Paper runs

### Run 3: REST feed, 10 min 9 s (16:14:27–16:24:36 IST, commit 3303999)
- 770 decisions, **770 rejected, 0 taken; 0 fills, 0 closed trades, 0 120 s exits observed**.
- Rejection reasons (a decision can carry several): INR metadata not fetched yet (strategy layer, before the governor) 462; at the governor: forecast uncalibrated 308, expected net edge ≤ 0 292, data integrity 7.
- Expected net edge over the 308 governor-evaluated candidates: min −33.4, median −16.2, max +21.6 bps (16 positive, all denied as uncalibrated). Round-trip cost = 2 × 5.9 bps taker + spread + 2 × 2 bps slippage.
- Forward calibration monitor (782 resolved): Brier 0.105 vs climatology 0.094, ECE 0.081. **The model is worse than the base rate.**
- Eligibility at INR 100 equity, 1 % risk (INR 1.00 budget): of 190 INR instruments with metadata, 1 eligible. The rest need INR 1.7–3.8 of risk at minimum order size, or returned null leverage. The budget is never raised.

### Run 4: WebSocket feed, 15 min (16:29:00–16:44 IST)
- Feed `ws`: currentPrices stream plus up to 8 live, sequence-checked WS books; 0 WS disconnects, 0 parse errors. 1,493 decisions used the WS book, 574 the REST fallback.
- 2,067 decisions, **all rejected; 0 trades**. Governor-evaluated: 829, all uncalibrated, 790 with net edge ≤ 0, 18 data-integrity. Net edge min −26.2, median −15.8, max +81.8 bps (39 positive, all denied).
- Forward monitor: 732 resolved, ECE 0.058, Brier 0.110 vs 0.102 climatology; 444 paths dropped for sample gaps (dropped, never guessed; cause not yet diagnosed).
- Incident: instrument records with `null` leverage fail to decode, and five consecutive decode failures opened the shared REST circuit breaker (58 errors: spot, universe and REST book fallback for ~30 s windows). Fixed in 72c8c7c: metadata uses its own client.

### Run 5: final binary (72c8c7c), WS feed, 10 min 25 s (16:44:56–16:55:22 IST)
- Feed `ws` throughout: 0 errors, 0 WS disconnects, 0 parse errors, 7 live books at the snapshot. 547 REST order-book bootstraps were all served, with 0 errors. With metadata on its own client, the breaker incident from run 4 did not recur.
- 1,422 decisions, **all rejected; 0 fills, 0 closed trades, 0 120 s exits observed**. Decisions used the WS book 878 times and the REST fallback 544 times.
- Rejection reasons (a decision can carry several):

  | Count | Reason |
  |---|---|
  | 1,208 | INR metadata not fetched yet, at the strategy layer |
  | 214 | Governor: forecast has no authority (registry *calibrating*) |
  | 205 | Governor: expected net edge ≤ 0 |
  | 1 | Governor: data integrity |

- Expected net edge over the 214 governor-evaluated candidates: min −33.6, median −14.2, max +87.9 bps. 9 were positive, and all 9 were denied authority.
- Forward monitor: 984 resolved, 0 dropped; ECE 0.080; Brier 0.103 vs climatology 0.096. It agrees with the s2-calib registry (ECE 0.080) that the model is worse than the base rate.
- Eligibility at INR 100 / 1 % risk: 0 of 159 INR instruments that had metadata at the snapshot were eligible.

## The 120 s exit
No position opened, so no live 120 s exit could be observed. The lifecycle uses an independent 250 ms server-side timer (`lifecycle_tick`). It is verified by the desk unit tests: time stop before 120 s, stop-loss exit, and flatten. The qe-risk lifecycle tests cover the 117.5 s lead and the hard 120,000 ms limit.

## s2-calib finding (relayed)
No model is calibrated. Selections with EV > 0 realised −8.9 to −13.6 bps per trade after 13.4 bps cost in s2-calib's walk-forward. So the desk is expected to take few or no trades, and it took none.

## What remains unverified
- The live paper lifecycle (entry → protection → exit) has not run against live data, because no entry was approved. It is covered by unit tests only.
- The liquidation estimate uses max(safety_percentage, smallest dynamic safety tier) as maintenance margin. This is not documented, so it is unverified.
- INR-margin P&L uses the spot USDTINR mid, which approximates the exchange's conversion rate.
- Run 4 dropped 444 forward-monitor paths for sample gaps. Run 5 dropped none, so the likely cause was the run-4 breaker outages, but this is not proven.
- Everything authenticated (s2-live) is built from the docs and has never been called.

## Implication for the 18,000 INR target
At INR 100 paper equity, the hard 1 % risk ceiling, live minimum order sizes and costs around 13 bps, the mathematics finds no admissible trade. The stochastic target simulation gives P(reach target) = 0.0 under the current model's win probability. A trade quota cannot override the governor, and none does.
