# Mathematical model (`qe-math`)

All functions are pure and deterministic. Decision outputs carry an `explain::Explanation`
(model id, formula, inputs, intermediates, outputs, notes), serialisable to deterministic JSON.

| Quantity | Formula | Module |
|---|---|---|
| Mid | M = (bid + ask)/2 | `micro` |
| Spread | (ask − bid)/M · 1e4 bps | `micro` |
| OBI_k | (ΣQb − ΣQa)/(ΣQb + ΣQa) over k ∈ {1,5,10,20} levels | `micro`, `features` |
| Microprice | (ask·Qb1 + bid·Qa1)/(Qb1 + Qa1) | `micro` |
| Book OFI (Cont–Kukanov–Stoikov) | e = 1[Pb′≥Pb]Qb′ − 1[Pb′≤Pb]Qb − 1[Pa′≤Pa]Qa′ + 1[Pa′≥Pa]Qa | `features::book_ofi` |
| Trade-flow imbalance | (V_buy − V_sell)/(V_buy + V_sell), aggressor-flagged volume only | `features` |
| Realised vol | σ₁ = √mean(r²) of gap-scaled 1s returns; σ_h = σ₁√h (historical, not forecast) | `features` |
| Momentum z | ln(P_now/P_{now−h}) / (σ₁√h) | `features` |
| EW stats | m ← m + a(x−m); v ← (1−a)(v + a(x−m_old)²) | `features::EwStats` |
| SL/TP | SL = k_s·σ₁₂₀, TP = k_t·σ₁₂₀ (log-return), long/short mirrored | `barrier` |
| First passage | binomial lattice of X_t = μt + σW_t: P(TP first), P(SL first), P(neither), E[hit time] | `prob` |
| Bracket EV | EV = Pw·W − Pl·L + Pn·E[R∣neither] − C; W, L gross, C charged once | `prob` |
| Sizing | R = E·f; N = R/((stop + cost)·fx), floored to increment; never rounded up to minimums | `sizing` |
| Target | n = ln(Ct/C0)/ln(1+r); TradesPerHour = n/H; scenarios at 5/10/15/20/30 per hour | `target`, `barrier` |

**Integrity rules.** Book OFI returns `None` when the snapshot version is not exactly
consecutive, the time gap exceeds 5 s, or either book is crossed or empty. Depth changes are
never invented. Stale data (> 5 s) raises the `stale_data` quality flag.

**Not yet implemented:** explicit regime classifier, Kalman/logistic/Bayesian forecasters,
walk-forward evaluation helper (see FORECASTING_MODEL.md).
