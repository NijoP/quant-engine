# Timeframe model (`qe-math::timeframe`)

Status: implemented and unit-tested; not yet run against live CoinDCX trades.

* **Bucketing**: epoch-aligned, `start = floor(ts/w)*w`, bucket covers `[start, start+w)`.
  Trades are sorted by event timestamp before bucketing.
* **1s candles** are built locally from timestamped trades (`candles_1s`). CoinDCX is not
  assumed to provide native 1s candles. Empty buckets are **not** synthesised.
* **Micro windows** 1, 5, 10, 15, 30, 60, 90, 120 s (`micro_windows`): trades in
  `(now-w, now]`; count, volume, buy/sell/unknown-side volume, VWAP, log return, intensity (trades/s).
  Aggressor side is taken only from the feed flag, never inferred.
* **Completeness**: a candle is `complete` iff `open + width <= now`. `completed()` filters;
  every downstream statistic uses completed candles only.
* **2d candles** (`aggregate_2d`): built only from two completed, contiguous 1d candles whose
  first day is aligned to a 2-day epoch boundary. A missing or incomplete day yields no 2d candle.
* **Context horizons** consumed from exchange candles: 3d, 1d, 1h, 15m, 5m, 1m (2d derived).
* **Gap-scaled returns**: `r_i / sqrt(dt_i)` so that tradeless seconds do not bias volatility.
