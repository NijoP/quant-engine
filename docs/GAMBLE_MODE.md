# Gamble mode

Gamble mode is an execution profile only the account owner can turn on. It
places bets without waiting for the forecast to prove a calibrated edge. It is
**off by default**. No target, schedule or agent can enable it.

> **Owner override (23 Nov 2026):** the owner chose to run gamble mode live for the 24 Nov campaign
> via `configs/campaign-live.toml` ([OWNER_START_0000.md](OWNER_START_0000.md)). The engineering
> verdict is NO-GO ([LIVE_GO_NO_GO.md](LIVE_GO_NO_GO.md)). The note below is the engineering recommendation.
>
> **Paper / research profile.** Gamble mode relaxes model authority: it skips the
> calibration and positive-net-EV gates. It is therefore **not** part of the
> 24 Nov campaign's live execution path ([CAMPAIGN_24NOV.md](CAMPAIGN_24NOV.md)),
> where both gates stay mandatory. Use it for paper simulation and research. Its
> owner floor is separate from the campaign's INR 95 floor and INR 1 / INR 5
> loss caps, and does not replace them. The live steps below describe code that
> exists at this commit. They are not a campaign recommendation.

## Honest expectation

- Round-trip cost is about 13-16 bps of notional (2 x taker fee + spread + 2 x slippage).
- The forecast has **no measured edge**. In the 30-min forward check the live
  Brier score was 0.1086 against 0.1025 for climatology, and mean predicted
  P(TP first) was 0.155 against a 0.116 hit rate.
- Each bet's expected value after costs is therefore usually **negative**.
  Gamble mode bets anyway because the owner chose to. The ranking below only
  picks the least-bad bet; it does not create an edge.
- Turning INR 100 into INR 50,000 needs a 500x gain, about 9 consecutive
  doublings. The dashboard shows the Monte Carlo probability
  (`/api/state.gamble.p_target`). At 10 INR x 10x it is effectively 0.

## What changes and what is kept

| | standard | gamble |
|---|---|---|
| calibration-authority gate | enforced | **skipped** |
| net EV > 0 after costs | enforced | **skipped** (EV still computed and shown) |
| session-loss / high-water-mark stop | enforced | replaced by the owner floor |
| sizing | 1% risk at stop | owner stake (or bold play) x owner leverage |
| leverage | highest exchange-permitted | owner leverage, capped at the exchange max for the side and notional tier |
| one open position | yes | yes |
| SL + TP | yes | yes (exchange-side via the PR 15 live path when live) |
| 120 s time exit | yes | yes |
| edge-lost early exit | yes | no: a bet is a pure bracket (SL / TP / 120 s / data integrity) |
| kill switch, data-integrity, metadata, min-notional, tradeability | yes | yes |
| stop beyond the estimated liquidation price | n/a | refused (lower the leverage) |
| martingale / averaging / size-up after a loss | never | never: a bet after a loss is never larger than the losing bet |
| decision snapshot before entry, post-trade record | yes | yes, flagged `mode=gamble` (`strategy_version desk.gamble.v1`, reasoning code `mode=gamble`; closed and decision records carry `"mode": "gamble"`) |

### Floor (permanent stop)

Isolated margin caps a bet's worst loss at its margin. Every bet is sized so that
`margin x (1 + leverage x cost) <= equity - floor`. So even a stop gapped through
to liquidation cannot take equity below the floor. A bet is never raised to meet
the exchange minimum. When equity minus the floor can no longer carry the smallest
bet (or equity is at or below the floor), gamble mode **stops for good**:

- entries are paused, and resuming is refused by the governor;
- `gamble-stopped.json` is written to the data dir. Startup refuses while it
  exists, until the owner restarts with `--gamble-rearm`.

When live, the same rule is checked against the **real wallet balance** before every
order. If real balance minus margin is below the floor, the order is refused and
gamble mode stops. A live trade that ends at or below the floor also stops it.

## How bets are chosen (intelligence and maths)

For every shortlisted instrument (top scanner candidates with fresh books), both
sides are estimated:

1. `p_model` = first-passage P(TP before SL within 120 s) from the existing
   forecast (drift from momentum + order-book imbalance, sigma from 1 s prices).
2. Global recalibration: once the forward monitor has >= 50 resolved samples,
   `p *= measured hit rate / mean predicted p` (the model currently over-predicts).
3. Empirical-Bayes shrink toward the resolved forward hit rate for **this
   instrument and side** (prior weight 30 samples).
4. Shrink toward realised gamble bets on this instrument and side (prior weight 10).
5. Payoff after costs: win = TP - cost, loss = SL + cost, time exit = E[R | neither] - cost.
   TP = 1.5 sigma sqrt(120 s) and SL = 1.0 sigma sqrt(120 s) scale with volatility, so
   the barriers are reachable in 120 s.
6. `EV = p_win W - p_stop L + p_neither N`, and `score = EV / L` (edge per unit of stop risk).

The side with the higher score is the candidate. Candidates sit on a ranking
board for 15 s. A candidate is **skipped** when its TP cannot cover the round-trip
cost, or when another fresh candidate scores higher. Only the best available bet is
taken. Liquidity and cost enter through the spread and fee in `cost`. The
`unstable` and `liquidity_withdrawal` regimes fail the data-integrity check.

## Maker entries (sprint 5)

Set `entry = "maker"` (and optionally `maker_timeout_s = 4`, default 4 s, max 30)
in the gamble config. The default is `entry = "taker"` (market order, as before).

- **Order:** a passive limit order at the near touch (buy at the best bid, sell at
  the best ask), `order_type = "limit_order"`, `time_in_force = "good_till_cancel"`.
  CoinDCX documents the futures `post_only` field as "Ignore this (Not supported
  at the moment)", so **no post-only flag is sent**. If the book moves through the
  price before the order arrives, the exchange can fill it as **taker**.
- **Timeout:** if the order is not filled within `maker_timeout_s`, the remainder
  is cancelled and the position is re-read once, to catch a fill that raced the
  cancel. A partial fill is protected (SL + TP) at its filled size. There is
  **no taker chase**. The desk evaluates again on the next scan.
- **Exits are unchanged:** exchange-side SL/TP when live, and the 120 s time exit
  as a market (taker) order.
- **Cost used for ranking and sizing:** `maker_fee + taker_fee + slippage` (the
  entry rests on the same side the exit leaves from, so no spread is crossed),
  against `2 x taker_fee + spread + 2 x slippage` for taker entries. Both fees
  come from live instrument metadata.
- **Paper simulator:** a resting entry fills only when public trades print
  **strictly through** its price after it was placed. A print *at* the price does
  not count, because the queue position is unknown and we assume we are last in
  the queue. The fill is at the limit price with the maker fee. When the timeout
  expires, the paper simulator fills what printed through (if that meets the
  exchange minimum quantity) and cancels the rest. Caveats: (1) real fills may
  be later or smaller, since queue position and hidden size are not modelled;
  (2) adverse selection is real, because a maker fill tends to come just as the
  price moves against the bet; this shows up in realised results, not in the cost.
- **Dashboard:** `/api/state.gamble.entry` and `/api/state.gamble.maker`
  (placed, filled, partial, cancelled, fill_rate).
- Only instruments whose metadata lists `limit_order` can be candidates.
- With a `[payoff]` section in the gamble config, `payoff.maker_entry` decides the
  entry order type, so the cost the plan ranks on always matches the order that
  is sent. If `entry` is also set and disagrees, startup is refused.

### Payoff wiring in the desk

When `[payoff]` is present, each evaluated instrument gets one
`qe_risk::gamble::payoff::plan` per side. Its inputs are the live maker/taker fee,
spread, top-5 depth on the exit side, the median 120 s high-low range of the last
~10 minutes of prices, sigma_120, the exchange max leverage, the maintenance
margin, and realised gamble bets on that pair and side. Excluded instruments
(for example cost > `max_cost_share_of_tp` of TP) are skipped with the plan's
reason. The chosen side uses the plan's TP/SL, cost and `win_p`, and the plan is
journaled on the decision (`payoff_plan`). The target-odds Monte Carlo then
takes `win_p`, the gross payoff ratio TP/SL and `cost_frac` from the best plan on
the board (`/api/state.gamble.simulation`, `barriers` = "payoff plan"). The
plan's liquidation-safe leverage is not passed to the governor. The governor
still uses the owner leverage capped at the exchange max, and refuses any stop
beyond the estimated liquidation price.

## Target probability

`/api/state.gamble.p_target` and `p_floor` come from `qe_math::gamble::target_odds`
(seeded Monte Carlo, 2,000 paths, refreshed every 10 s). It uses the same floor,
the leverage cap and the exchange minimum notional:

- the win rate and payoff ratio come from realised gamble bets once >= 10 exist;
  before that, from the best current candidate's estimate (P(win) conditional on a
  barrier being hit; time exits are not modelled separately);
- bets remaining = hours to the deadline x bets per hour. The deadline is
  `stage2_deadline_ms` from `configs/risk-limits.toml`. When this was written it
  was 17:00 IST 24 Nov, which is now **historical**: the campaign deadline is
  23:00 IST (23-hour schedule; the 22:00 IST deadline is superseded). Bets per hour is the
  measured rate once >= 5 bets have closed; otherwise 20/h, which assumes one
  position and holds of 120 s or less.

### Stake policies

- `fixed`: margin per bet = `stake_inr`, capped by the floor rule.
- **24 Nov campaign bold play (current, live-capable under owner arming):** `configs/campaign-live.toml` keeps `stake_policy = "fixed"` and sets `campaign_stake = "next_milestone"`. Each bet's margin is sized so one TP win reaches the next hourly milestone, limited to the equity above the required owner floor. This is the profile in [OWNER_START_0000.md](OWNER_START_0000.md). The `target_optimal` restriction below applies only to that older policy.
- `target_optimal` (bold play, older policy): the largest bet the floor rule, leverage cap and
  exchange limits allow, unless a smaller bet reaches the target with one win.
  In a game with negative expectation this maximises P(reach a fixed target),
  because fewer bets means less exposure to the house edge. Exception: the
  no-size-up-after-a-loss rule still applies, so it differs from textbook bold
  play near the target. It is **paper only** in this release, because it sizes
  from desk equity rather than the real wallet.

## Owner steps to enable gamble mode

1. Edit `configs/gamble.toml` (or point `QE_GAMBLE_CONFIG` at your own copy):
   ```toml
   enabled = true
   owner_ack = "I ACCEPT: gamble mode has no proven edge and can lose my stake down to the floor"
   stake_inr = "10"
   leverage = 10
   floor_inr = "50"
   stake_policy = "fixed"      # or "target_optimal" (paper only)
   execution_mode = "paper"
   ```
   If any of `stake_inr`, `leverage` or `floor_inr` is missing, startup is refused.
   There are no trading defaults.
2. Paper: `qe-desk --profile gamble` (or `QE_PROFILE=gamble`). The data goes to
   `data/desk-gamble` unless `QE_DATA_DIR` is set.
3. Live (real money): set `execution_mode = "live"` in the gamble config, arm
   through the existing owner path (`qe-live-check arm`, docs/GO_LIVE.md), and start
   with `QE_EXECUTION_MODE=live qe-desk --profile gamble`. If the config and the
   start mode disagree, startup is refused. If the arm is missing or expired, the desk
   stays on paper.
4. Stop at any time: use the dashboard kill switch (`POST /api/control/kill`), or
   `qe-live-check kill` when live.
5. After a floor stop: review, then restart with `--gamble-rearm`.
