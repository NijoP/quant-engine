//! The real `PrivateClient` against a local mock HTTP server that serves the
//! documented example responses (docs.coindcx.com, 2026-11-23). Proves the
//! read paths decode the documented shapes, that requests are signed, and that
//! no mutation request ever leaves the process without an open gate.
use qe_coindcx::auth::{Credentials, HEADER_SIGNATURE};
use qe_coindcx::live::{LiveGate, OwnerSession};
use qe_coindcx::private::PrivateClient;
use qe_live_check::checkpoint;
use qe_live_check::readiness::{self, DeskState, MARGINS};
use qe_live_check::Poll;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Seen = Arc<Mutex<Vec<(String, bool)>>>;

fn respond(path: &str) -> &'static str {
    match path {
        // Docs: Futures "Wallet details" example response.
        "/exchange/v1/derivatives/futures/wallets" => {
            r#"[{"id":"c5f039dd-4e11-4304-8f91-e9c1f62d754d","currency_short_name":"USDT","balance":"6.1693226","locked_balance":"0.0","cross_order_margin":"0.0","cross_user_margin":"0.68534648"},{"id":"x","currency_short_name":"INR","balance":"98.5","locked_balance":"1.5","cross_order_margin":"0.0","cross_user_margin":"0.0"}]"#
        }
        // Docs: "List positions" example (flat position).
        "/exchange/v1/derivatives/futures/positions" => {
            r#"[{"id":"571eae12-236a-11ef-b36f-83670ba609ec","pair":"B-BNB_USDT","active_pos":0.0,"inactive_pos_buy":0.0,"inactive_pos_sell":0.0,"avg_price":0.0,"liquidation_price":0.0,"locked_margin":0.0,"locked_user_margin":0.0,"locked_order_margin":0.0,"take_profit_trigger":null,"stop_loss_trigger":null,"leverage":10.0,"maintenance_margin":0.0,"mark_price":0.0,"margin_type":"crossed","margin_currency_short_name":"USDT","updated_at":1717754279737}]"#
        }
        "/exchange/v1/derivatives/futures/orders" => "[]",
        // Docs: spot "Get balances" example + an INR row.
        "/exchange/v1/users/balances" => {
            r#"[{"currency":"BTC","balance":1.167,"locked_balance":2.1},{"currency":"INR","balance":"40.25","locked_balance":"0"}]"#
        }
        _ => r#"{"message":"success","status":200,"code":200}"#,
    }
}

async fn serve() -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let s2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                return;
            };
            let seen = s2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head_end, len) = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let first = head.lines().next().unwrap_or("").to_string();
                let path = first.split(' ').nth(1).unwrap_or("").to_string();
                let signed = head
                    .to_lowercase()
                    .contains(&format!("{}:", HEADER_SIGNATURE.to_lowercase()));
                seen.lock().unwrap().push((first, signed));
                let body = respond(&path);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

fn client(base: &str, gate: LiveGate) -> PrivateClient {
    PrivateClient::new(Credentials::new("k".into(), "s".into()).unwrap(), gate).with_base_url(base)
}

#[tokio::test]
async fn readiness_and_balances_decode_documented_responses() {
    let (base, seen) = serve().await;
    let pc = client(&base, LiveGate::default());
    let (st, snap) = readiness::account_check(&pc, &DeskState::default(), 1).await;
    assert!(st.is_pass(), "{st:?}");
    let snap = snap.unwrap();
    assert!(snap.is_flat() && snap.wallets.len() == 2);
    let b = checkpoint::read_balances(&pc, 2).await.unwrap();
    let d = |s| rust_decimal::Decimal::from_str(s).unwrap();
    assert_eq!(b.futures["INR"], d("100"));
    assert_eq!(b.futures["USDT"], d("6.1693226"));
    assert_eq!(b.spot["INR"], d("40.25"));
    assert_eq!(b.total("INR"), d("140.25"));
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|(_, signed)| *signed),
        "every private request is signed"
    );
    assert!(seen
        .iter()
        .any(|(l, _)| l.starts_with("GET /exchange/v1/derivatives/futures/wallets")));
    assert!(seen
        .iter()
        .any(|(l, _)| l.starts_with("POST /exchange/v1/users/balances")));
}

#[tokio::test]
async fn no_mutation_leaves_the_process_without_the_gate() {
    let (base, seen) = serve().await;
    // Default (paper) gate, and a checkpoint session in a build without the
    // live-orders feature: flatten attempts must never reach the server.
    for gate in [
        LiveGate::default(),
        LiveGate::owner_session(OwnerSession::Checkpoint, true),
    ] {
        if LiveGate::COMPILED && gate.owner_session.is_some() {
            continue; // with the feature compiled the checkpoint session may reduce risk
        }
        let pc = client(&base, gate);
        let rep = checkpoint::flatten_all(&pc, &MARGINS, Poll::IMMEDIATE).await;
        assert!(rep.errors.iter().any(|e| e.contains("gate")), "{rep:?}");
        // Reads still work and show the (mock) account flat.
        assert!(rep.flat);
    }
    let seen = seen.lock().unwrap().clone();
    let mutations: Vec<_> = seen
        .iter()
        .filter(|(l, _)| {
            l.contains("cancel")
                || l.contains("exit")
                || l.contains("create")
                || l.contains("leverage")
        })
        .collect();
    assert!(
        mutations.is_empty(),
        "mutations reached the wire: {mutations:?}"
    );
}
