# Compliance checklist (for the account owner)

This is an engineering checklist, **not legal advice**, and it reaches no conclusion
about whether any activity is permitted, exempt or authorised. It quotes the CoinDCX
API License Terms and FAQ as published at https://docs.coindcx.com/ (read 2026-11-23).
Check the current text yourself before running anything live.

## What the CoinDCX API terms say (summary with clause numbers)

| Topic | Clause | Summary |
| --- | --- | --- |
| Definitions | 1.4, 1.11 | Defines "Algorithmic Trading" (automated) and "High-Frequency Trading" (high speed, high turnover, high order-to-trade ratio). |
| Licence | 2.1-2.2 | Non-exclusive, revocable, restricted licence for authorised use only, under applicable law. |
| Market data | 2.3(c) | You may not redistribute, display or disseminate Market Data, or works derived from it, to any third party. |
| No tampering | 2.3(a)-(b) | No altering or misrepresenting API/Market Data, and no reverse engineering. |
| Fees | 3.1 | No API fees at present. CoinDCX may introduce them. |
| Termination | 5.2-5.3 | CoinDCX may terminate access without notice, including for breach. |
| Licences and approvals | 6.1, 6.4 | You represent that you hold the required authority and any licences needed for algorithmic or HFT trading under applicable law. |
| Record retention | 6.6 | Keep information, data and records about your use of the API and Market Data for **5 years** after the terms end. |
| No advice | 6.7 | CoinDCX gives no investment, tax or legal advice. You alone are responsible for your decisions and compliance. |
| Rate limits (FAQ) | FAQ | 16 requests/s and 960/min. |
| API key permissions (FAQ) | FAQ | All API keys carry the same permissions. Read-only keys are not offered. |
| Futures via public API (FAQ) | FAQ | The FAQ says futures trading is not available through public APIs, yet authenticated futures endpoints are documented. Confirm your account's API futures access with CoinDCX. |

## Checklist before any live operation

- [ ] I have read the current CoinDCX API License Terms and Terms of Use.
- [ ] I have checked, with qualified advice if needed, whether automated or high-frequency trading needs a licence, registration or approval where I live. (Clause 6.4)
- [ ] I have confirmed with CoinDCX that my account may trade futures through the API (see the FAQ conflict above).
- [ ] My API key is IP-bound where possible, stored only in env or a `chmod 600` file, and has never appeared in chat, screenshots, tickets or Git.
- [ ] I keep a 5-year archive of orders, fills, fees, P&L and API usage records (clause 6.6). `audit/` is gitignored; back it up somewhere private.
- [ ] I will not redistribute CoinDCX market data, or dashboards and analytics derived from it, to third parties (clause 2.3(c)). The dashboard stays private.
- [ ] I understand that futures with leverage can lose more than the planned stop distance through gaps, slippage or liquidation, and that no forecast in this system is a guarantee.
- [ ] I ran the `qe-live-check` sequence in `docs/LIVE_READINESS.md` myself (readiness, governor, supervised `validate`, readiness, `arm`), and every item is PASS.
- [ ] I flattened all positions and cancelled all orders before the first live session (the readiness check fails while exposure exists).
- [ ] I alone set `QE_EXECUTION_MODE=live` and `QE_LIVE_OPERATOR_CONFIRM`. No automated process sets them.
- [ ] I know how to stop everything: `qe-live-check kill`, then `qe-live-check checkpoint --kind adhoc`. If that cannot confirm flat, use the CoinDCX app to cancel orders and exit positions.
- [ ] I do the 23:00 IST 24 Nov campaign final checkpoint (the 22:00 IST deadline is superseded) with `qe-live-check checkpoint` ([CAMPAIGN_24NOV.md](CAMPAIGN_24NOV.md)). The 22:30 checkpoint, the INR 2,000 withdrawal and the 17:00 deadline are historical. Any withdrawal is mine to do manually in the CoinDCX app. `data/live/` (audit log and reports) is part of my 5-year archive.

## Engineering commitments in this repository

* No withdrawal, wallet transfer or add-margin endpoint is implemented. `qe-live-check` only reads balances around the owner's manual withdrawal.
* Paper equity is always labelled as paper. No balance is ever fabricated.
* Risk ceilings: see `docs/RISK_POLICY.md`; the live gate never relaxes them.
