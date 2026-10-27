/// gap_test — Fire-and-forget place → sleep(gap) → cancel.
/// Measures the real on-book time by sweeping gap values.
/// Cancel success = server processed the place before the cancel arrived.

use std::io::Write;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::dyn_abi::Eip712Domain;
use alloy::primitives::U256;
use alloy::signers::Signer;
use alloy::sol_types::SolStruct;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use polymarket_client_sdk::POLYGON;
use polymarket_client_sdk::auth::LocalSigner;
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::clob::types::Side;

use rust_decimal::Decimal;
use tokio::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

// ── Shared state for BBO ────────────────────────────────────────────────────

struct Shared {
    mid: Mutex<Option<f64>>,
    kill: Mutex<bool>,
}

// ── Logging ─────────────────────────────────────────────────────────────────

fn log(file: &mut std::fs::File, line: &str) {
    let ts = Utc::now().format("%H:%M:%S%.3f");
    let msg = format!("[{}] {}", ts, line);
    println!("{}", msg);
    writeln!(file, "{}", msg).ok();
}

// ── HMAC auth (signer address for everything) ───────────────────────────────

fn hmac_headers(
    api_key: &str,
    api_secret: &str,
    passphrase: &str,
    address: &str,
    method: &str,
    path: &str,
    body: &str,
) -> Vec<(String, String)> {
    let timestamp = Utc::now().timestamp();
    let message = format!("{}{}{}{}", timestamp, method, path, body);
    let decoded = URL_SAFE.decode(api_secret)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(api_secret))
        .expect("decode secret");
    let mut mac = HmacSha256::new_from_slice(&decoded).unwrap();
    mac.update(message.as_bytes());
    let sig = URL_SAFE.encode(mac.finalize().into_bytes());
    vec![
        ("POLY_ADDRESS".into(), address.into()),
        ("POLY_API_KEY".into(), api_key.into()),
        ("POLY_PASSPHRASE".into(), passphrase.into()),
        ("POLY_SIGNATURE".into(), sig),
        ("POLY_TIMESTAMP".into(), timestamp.to_string()),
    ]
}

// ── Manual place (returns order_id before sending, sends in background) ─────

struct PlaceResult {
    order_id: String,
    body_str: String,
}

/// Build + sign order, compute order_id, serialize with postOnly.
/// Does NOT send — caller decides when to fire.
async fn prepare_order_async(
    sdk: &Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
    signer: &LocalSigner<alloy::signers::k256::ecdsa::SigningKey>,
    domain: &Eip712Domain,
    token: &str,
    price: Decimal,
    size: Decimal,
) -> Result<PlaceResult, String> {
    let order = sdk.limit_order()
        .token_id(token).size(size).price(price).side(Side::Buy)
        .build().await
        .map_err(|e| format!("build: {e}"))?;

    let signed = sdk.sign(signer, order).await
        .map_err(|e| format!("sign: {e}"))?;

    let hash = signed.order.eip712_signing_hash(domain);
    let order_id = format!("0x{}", hex::encode(hash));

    let mut body_val = serde_json::to_value(&signed)
        .map_err(|e| format!("ser: {e}"))?;
    if let serde_json::Value::Object(ref mut map) = body_val {
        map.insert("postOnly".into(), serde_json::Value::Bool(true));
    }
    let body_str = serde_json::to_string(&body_val).unwrap();

    Ok(PlaceResult { order_id, body_str })
}

/// Fire a place request. Returns (status_code, success).
async fn fire_place(
    http: &reqwest::Client,
    body_str: &str,
    api_key: &str,
    api_secret: &str,
    passphrase: &str,
    address: &str,
) -> (u16, bool) {
    let path = "/order";
    let headers = hmac_headers(api_key, api_secret, passphrase, address, "POST", path, body_str);
    let url = "https://clob.polymarket.com/order";

    let mut req = http.post(url)
        .header("Content-Type", "application/json")
        .body(body_str.to_string());
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    match req.send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            (code, code >= 200 && code < 300)
        }
        Err(_) => (0, false),
    }
}

/// Fire a cancel request. Returns (status_code, success).
async fn fire_cancel(
    http: &reqwest::Client,
    order_id: &str,
    api_key: &str,
    api_secret: &str,
    passphrase: &str,
    address: &str,
) -> (u16, bool) {
    let path = "/order";
    let body = serde_json::json!({"orderID": order_id});
    let body_str = serde_json::to_string(&body).unwrap();
    let headers = hmac_headers(api_key, api_secret, passphrase, address, "DELETE", path, &body_str);
    let url = "https://clob.polymarket.com/order";

    let mut req = http.delete(url)
        .header("Content-Type", "application/json")
        .body(body_str);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    match req.send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            (code, code >= 200 && code < 300)
        }
        Err(_) => (0, false),
    }
}

/// Fire cancel-all.
async fn fire_cancel_all(
    http: &reqwest::Client,
    api_key: &str,
    api_secret: &str,
    passphrase: &str,
    address: &str,
) -> bool {
    let path = "/cancel-all";
    let body_str = "";
    let headers = hmac_headers(api_key, api_secret, passphrase, address, "DELETE", path, body_str);
    let url = "https://clob.polymarket.com/cancel-all";

    let mut req = http.delete(url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    match req.send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

// ── CLOB BBO (same as freq_test) ────────────────────────────────────────────

async fn run_clob_bbo(
    shared: Arc<Shared>,
    yes_token: String,
    kill_low: f64,
    kill_high: f64,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    loop {
        let connect = tokio_tungstenite::connect_async(
            "wss://ws-subscriptions-clob.polymarket.com/ws/market"
        ).await;
        let (mut ws, _) = match connect {
            Ok(c) => c,
            Err(_) => { tokio::time::sleep(Duration::from_secs(2)).await; continue; }
        };

        let sub = serde_json::json!({
            "type": "market", "assets_ids": [&yes_token],
            "custom_feature_enabled": true
        });
        let _ = ws.send(Message::Text(sub.to_string())).await;

        let mut bids: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut asks: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        while let Some(Ok(msg)) = ws.next().await {
            if *shared.kill.lock().await { return; }
            let text = match msg {
                Message::Text(t) => t,
                Message::Ping(d) => { let _ = ws.send(Message::Pong(d)).await; continue; }
                _ => continue,
            };
            let d: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) if v.is_object() => v,
                _ => continue,
            };

            let evt = d.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
            let aid = d.get("asset_id").and_then(|v| v.as_str()).unwrap_or("");

            if evt == "book" && aid == yes_token {
                bids.clear(); asks.clear();
                for b in d.get("bids").or(d.get("buys")).and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if let (Some(p), Some(s)) = (b.get("price").and_then(|v| v.as_str()), b.get("size").and_then(|v| v.as_str())) {
                        bids.insert(p.into(), s.into());
                    }
                }
                for a in d.get("asks").or(d.get("sells")).and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if let (Some(p), Some(s)) = (a.get("price").and_then(|v| v.as_str()), a.get("size").and_then(|v| v.as_str())) {
                        asks.insert(p.into(), s.into());
                    }
                }
            } else if evt == "price_change" {
                for pc in d.get("price_changes").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if pc.get("asset_id").and_then(|v| v.as_str()).unwrap_or("") != yes_token { continue; }
                    let p = pc.get("price").and_then(|v| v.as_str()).unwrap_or("0");
                    let sz = pc.get("size").and_then(|v| v.as_str()).unwrap_or("0");
                    let side = pc.get("side").and_then(|v| v.as_str()).unwrap_or("");
                    let map = if side == "BUY" { &mut bids } else { &mut asks };
                    if sz.parse::<f64>().unwrap_or(0.0) == 0.0 { map.remove(p); }
                    else { map.insert(p.into(), sz.into()); }
                }
            }

            if !bids.is_empty() && !asks.is_empty() {
                let bb: f64 = bids.keys().filter_map(|p| p.parse().ok()).fold(0.0_f64, f64::max);
                let ba: f64 = asks.keys().filter_map(|p| p.parse().ok()).fold(f64::MAX, f64::min);
                let mid = (bb + ba) / 2.0;
                *shared.mid.lock().await = Some(mid);
                if mid < kill_low || mid > kill_high {
                    *shared.kill.lock().await = true;
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    let mut logfile = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open("gap_test.log").unwrap();

    log(&mut logfile, "═══ GAP TEST ═══");

    let pk = std::env::var("PRIVATE_KEY").expect("PRIVATE_KEY");
    let funder = std::env::var("FUNDER_ADDRESS").expect("FUNDER_ADDRESS");
    let api_key = std::env::var("API_KEY").expect("API_KEY");
    let api_secret = std::env::var("API_SECRET").expect("API_SECRET");
    let passphrase = std::env::var("PASSPHRASE")
        .or_else(|_| std::env::var("API_PASSPHRASE")).expect("PASSPHRASE");
    let kill_low: f64 = std::env::var("KILL_LOW").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(0.20);
    let kill_high: f64 = std::env::var("KILL_HIGH").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(0.80);
    let cycles_per_gap: usize = std::env::var("CYCLES").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(10);

    // Gaps to test (ms)
    let gaps: Vec<u64> = std::env::var("GAPS").ok()
        .map(|s| s.split(',').filter_map(|g| g.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![10, 5, 3, 1, 0]);

    let signer = LocalSigner::from_str(&pk).expect("Bad key")
        .with_chain_id(Some(POLYGON));
    let sdk = Client::new("https://clob.polymarket.com", Config::default()).unwrap()
        .authentication_builder(&signer)
        .signature_type(polymarket_client_sdk::clob::types::SignatureType::Proxy)
        .funder(funder.parse().unwrap())
        .authenticate().await.unwrap();
    let domain = Eip712Domain {
        name: Some("Polymarket CTF Exchange".into()),
        version: Some("1".into()),
        chain_id: Some(U256::from(137u64)),
        verifying_contract: Some("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E".parse().unwrap()),
        ..Default::default()
    };
    let address = format!("0x{}", hex::encode(signer.address()));
    log(&mut logfile, &format!("Auth addr={}", address));

    let http = reqwest::Client::builder()
        .use_rustls_tls()
        .http2_adaptive_window(true)
        .pool_max_idle_per_host(4)
        .build().unwrap();

    // Fetch window + tokens
    let now_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let wts = now_sec - (now_sec % 300);
    let window_end = (wts + 300) * 1000;

    let gamma: serde_json::Value = http
        .get(format!("https://gamma-api.polymarket.com/events?slug=btc-updown-5m-{}&closed=false", wts))
        .header("User-Agent", "Mozilla/5.0")
        .send().await.unwrap().json().await.unwrap();
    let tids: Vec<String> = serde_json::from_str(
        gamma[0]["markets"][0]["clobTokenIds"].as_str().unwrap()
    ).unwrap();
    let token_yes = tids[0].clone();

    let remaining = 300 - (now_sec - wts);
    log(&mut logfile, &format!("Window {} | ...{} | {}s left", wts, &token_yes[token_yes.len()-8..], remaining));

    // Warm cache
    let price = Decimal::from_str("0.01").unwrap();
    let size = Decimal::from(5i64);
    let _ = sdk.limit_order().token_id(&token_yes).size(size).price(price)
        .side(Side::Buy).build().await;
    log(&mut logfile, "Cache warm");

    // BBO
    let shared = Arc::new(Shared { mid: Mutex::new(None), kill: Mutex::new(false) });
    let bbo_handle = {
        let s = shared.clone(); let t = token_yes.clone();
        tokio::spawn(async move { run_clob_bbo(s, t, kill_low, kill_high).await; })
    };
    log(&mut logfile, "Waiting for BBO...");
    loop {
        if shared.mid.lock().await.is_some() { break; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mid = shared.mid.lock().await.unwrap();
    log(&mut logfile, &format!("BBO ready mid={:.4}", mid));

    // ── Sweep gaps ──────────────────────────────────────────────────────
    for &gap_ms in &gaps {
        if *shared.kill.lock().await { break; }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        if now_ms >= window_end { break; }

        log(&mut logfile, &format!("\n── GAP = {}ms ({} cycles) ──", gap_ms, cycles_per_gap));
        log(&mut logfile, &format!(
            "{:>4} {:>6} {:>8} {:>8} {:>8} {:>8}  {}",
            "#", "gap", "pl_send", "cn_send", "pl_code", "cn_code", "result"
        ));

        let mut place_ok = 0;
        let mut cancel_ok = 0;
        let mut cancel_not_found = 0;
        let mut cancel_other_fail = 0;

        for cycle in 1..=cycles_per_gap {
            if *shared.kill.lock().await { break; }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
            if now_ms >= window_end { break; }

            // Pre-build order
            let prepared = match prepare_order_async(
                &sdk, &signer, &domain, &token_yes, price, size
            ).await {
                Ok(p) => p,
                Err(e) => {
                    log(&mut logfile, &format!("{:>4} {:>6} BUILD_FAIL: {}", cycle, gap_ms, e));
                    continue;
                }
            };

            let oid = prepared.order_id.clone();

            // ── FIRE PLACE (spawn, don't wait) ──────────────────────
            let t_place_send = Instant::now();
            let place_handle = {
                let http = http.clone();
                let body = prepared.body_str.clone();
                let ak = api_key.clone();
                let as_ = api_secret.clone();
                let pp = passphrase.clone();
                let addr = address.clone();
                tokio::spawn(async move {
                    fire_place(&http, &body, &ak, &as_, &pp, &addr).await
                })
            };

            // ── SLEEP THE GAP ───────────────────────────────────────
            if gap_ms > 0 {
                tokio::time::sleep(Duration::from_millis(gap_ms)).await;
            }

            // ── FIRE CANCEL (spawn, don't wait) ─────────────────────
            let t_cancel_send = Instant::now();
            let gap_actual_ms = t_place_send.elapsed().as_secs_f64() * 1000.0;
            let cancel_handle = {
                let http = http.clone();
                let oid = oid.clone();
                let ak = api_key.clone();
                let as_ = api_secret.clone();
                let pp = passphrase.clone();
                let addr = address.clone();
                tokio::spawn(async move {
                    fire_cancel(&http, &oid, &ak, &as_, &pp, &addr).await
                })
            };

            // ── Wait for both to complete ────────────────────────────
            let (pl_code, pl_ok) = place_handle.await.unwrap_or((0, false));
            let (cn_code, cn_ok) = cancel_handle.await.unwrap_or((0, false));

            let pl_ms = t_place_send.elapsed().as_secs_f64() * 1000.0;
            let cn_ms = t_cancel_send.elapsed().as_secs_f64() * 1000.0;

            if pl_ok { place_ok += 1; }
            if cn_ok { cancel_ok += 1; }
            else if cn_code == 404 { cancel_not_found += 1; }
            else { cancel_other_fail += 1; }

            let result = match (pl_ok, cn_ok) {
                (true, true) => "place+cancel OK",
                (true, false) => if cn_code == 404 { "place OK, cancel 404 (too fast)" } else { "place OK, cancel FAIL" },
                (false, _) => "place FAIL",
            };

            log(&mut logfile, &format!(
                "{:>4} {:>5.1} {:>8.1} {:>8.1} {:>8} {:>8}  {}",
                cycle, gap_actual_ms, pl_ms, cn_ms, pl_code, cn_code, result
            ));

            // ── Safety: cancel_all after each cycle ──────────────────
            fire_cancel_all(&http, &api_key, &api_secret, &passphrase, &address).await;

            // Brief pause between cycles for capital release
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        log(&mut logfile, &format!(
            "  Gap {}ms: {}/{} placed, {}/{} cancelled, {} not_found(too fast), {} other_fail",
            gap_ms, place_ok, cycles_per_gap, cancel_ok, cycles_per_gap,
            cancel_not_found, cancel_other_fail
        ));
    }

    // ── Final cleanup ───────────────────────────────────────────────────
    fire_cancel_all(&http, &api_key, &api_secret, &passphrase, &address).await;
    *shared.kill.lock().await = true;
    let _ = bbo_handle.await;

    log(&mut logfile, "\n═══ GAP TEST DONE ═══");
}
