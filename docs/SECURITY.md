# Security

This repository is public. Assume every committed file is read by anyone.

## Credentials

| Rule | How it is enforced |
| --- | --- |
| Public market data needs no key | `PublicClient` never signs anything; the desk scans with no credentials. |
| Only the backend loads credentials, at runtime | `qe_coindcx::auth::load_credentials()` reads `COINDCX_API_KEY` / `COINDCX_API_SECRET` from the environment, else from `QE_SECRETS_FILE` (default `./.env`). |
| The secrets file must be private | On Unix the loader refuses any file with group/other permission bits (`chmod 600`). |
| Never in Git | `.env` and `.env.*` are gitignored; `.env.example` carries names only. |
| Never in logs, prompts, screenshots or the frontend | `Credentials`, `SignedBody` and the private-socket `JoinFrame` have redacted `Debug` output; the HTTP client logs no bodies or headers; status surfaces expose only `CredentialStatus { configured, source, problem }`. |
| Never verified by printing | `live_readiness` reports `configured (env|file)` / `not configured` only. |
| No research agent or subagent receives credentials | Credentials live only in the `PrivateClient` process; nothing serialises them. |

CoinDCX states that read-only API keys are not available. A configured key can place
orders and move funds, so:

* bind the key to your IP address in CoinDCX settings where possible;
* do not put the key on a shared or remote machine;
* regenerate the key if it was ever pasted into a chat, ticket, screenshot or commit.

## Request signing

`signature = hex(HMAC-SHA256(API secret, compact JSON body))`, sent with
`X-AUTH-APIKEY` and `X-AUTH-SIGNATURE`. The client signs the exact byte string it sends
(`Credentials::sign_body`). The request timestamp comes from one function
(`auth::request_timestamp_ms`) and is in milliseconds. The docs' field tables say
seconds, but every official example uses milliseconds. That choice is
**unverified** until the account owner's `live_readiness --read-account` wallet fetch
succeeds. `auth::signing_self_test()` checks RFC 4231 plus vectors computed with
Python's `hmac` for the documented cancel-order body and the private-socket payload.

## Live execution gate

Order-changing calls (`update_leverage`, create order, cancel, `create_tpsl`, exit,
cancel-all-for-position) go through `live::LiveGate::check()` and are refused unless
**all** of these hold:

1. built with `--features live-orders` (off by default);
2. `QE_EXECUTION_MODE=live` (any other value means paper);
3. `QE_LIVE_OPERATOR_CONFIRM` set by the account owner to the exact phrase in
   `live::OPERATOR_CONFIRM_PHRASE`;
4. credentials configured;
5. every readiness item passes: credentials, signing self-test, account reconciled,
   instrument requirements, protective orders verified, 120 s closure tested, and
   risk-governor acceptance.

In this build, items "protective orders verified", "120 s closure tested" and
"risk-governor acceptance" are `NotRun` because a read-only command cannot establish
them. **So the gate cannot open, and live trading is disabled.** No schedule, target
controller, agent or prompt can change that. Only the account owner can, after a
supervised validation that is not built yet.

## Failure handling on authenticated calls

* Mutations are never retried automatically. A timeout, 5xx or unreadable response
  after sending becomes `PrivateError::OutcomeUnknown`, and the order ledger refuses
  any new order until `reconcile_order` has matched the exchange's order list.
  A retry then needs a new client order id.
* HTTP 429 puts the client into a cooldown that starts at 2 s and doubles up to
  120 s. Five consecutive failures open a 30 s circuit breaker.
* The client limits itself to 8 requests/s and 480/min globally, half the
  documented 16/s and 960/min, with tighter per-endpoint budgets.

## Reporting

Report vulnerabilities privately to the repository owner. Do not open a public issue.
