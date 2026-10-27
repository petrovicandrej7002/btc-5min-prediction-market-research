/// freq_test v2 — All HTTP (place/cancel/cancel-all) via manual HMAC with signer address.
/// SDK only used for: auth, order building, order signing.

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
const CLOB: &str = "https://clob.polymarket.com";

// ── Shared state ────────────────────────────────────────────────────────────
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

// ── HMAC headers (matches SDK auth::l2 exactly) ────────────────────────────
fn hmac_headers(
    api_key: &str, api_secret: &str, passphrase: &str, address: &str,
    method: &str, path: &str, body: &str,
) -> Vec<(String, String)> {
    let timestamp = Utc::now().timestamp();
    let message = format!("{}{}{}{}", timestamp, method, path, body);
    let secret = URL_SAFE.decode(api_secret)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(api_secret))
        .expect("bad API_SECRET");
    let mut mac = HmacSha256::new_from_slice(&secret).unwrap();
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

// ── Auth context (credentials for all manual HTTP calls) ────────────────────
#[derive(Clone)]
struct Auth {
    api_key: String,
    api_secret: String,
    passphrase: String,
    address: String, // signer address, 0x-prefixed
}

// ── Manual place with postOnly ──────────────────────────────────────────────
async fn manual_place(
    http: &reqwest::Client, auth: &Auth,
    sdk: &Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
    signer: &LocalSigner<alloy::signers::k256::ecdsa::SigningKey>,
    domain: &Eip712Domain,
    token: &str, price: Decimal, size: Decimal,
) -> (String, bool, String) {
    let order = match sdk.limit_order()
        .token_id(token).size(size).price(price).side(Side::Buy)
        .build().await
    {
        Ok(o) => o,
        Err(e) => return ("".into(), false, format!("build: {e}")),
    };
    let signed = match sdk.sign(signer, order).await {
        Ok(s) => s,
        Err(e) => return ("".into(), false, format!("sign: {e}")),
    };
    let hash = signed.order.eip712_signing_hash(domain);
    let order_id = format!("0x{}", hex::encode(hash));

    let mut body_val = match serde_json::to_value(&signed) {
        Ok(v) => v,
        Err(e) => return (order_id, false, format!("ser: {e}")),
    };
    if let serde_json::Value::Object(ref mut map) = body_val {
        map.insert("postOnly".into(), serde_json::Value::Bool(true));
    }
    let body_str = serde_json::to_string(&body_val).unwrap();

    let path = "/order";
    let hdrs = hmac_headers(&auth.api_key, &auth.api_secret, &auth.passphrase, &auth.address, "POST", path, &body_str);
    let mut req = http.post(format!("{}{}", CLOB, path))
        .header("Content-Type", "application/json").body(body_str);
    for (k, v) in &hdrs { req = req.header(k.as_str(), v.as_str()); }

    match req.send().await {
        Ok(r) => {
            let st = r.status(); let txt = r.text().await.unwrap_or_default();
            if st.is_success() { (order_id, true, "ok".into()) }
            else { (order_id, false, format!("{} {}", st.as_u16(), txt.chars().take(200).collect::<String>())) }
        }
        Err(e) => (order_id, false, format!("net: {e}")),
    }
}

// ── Manual cancel single order ──────────────────────────────────────────────
async fn manual_cancel(http: &reqwest::Client, auth: &Auth, order_id: &str) -> (bool, String) {
    let path = "/order";
    let body = serde_json::json!({"orderID": order_id});
    let body_str = serde_json::to_string(&body).unwrap();
    let hdrs = hmac_headers(&auth.api_key, &auth.api_secret, &auth.passphrase, &auth.address, "DELETE", path, &body_str);
    let mut req = http.delete(format!("{}{}", CLOB, path))
        .header("Content-Type", "application/json").body(body_str);
    for (k, v) in &hdrs { req = req.header(k.as_str(), v.as_str()); }
    match req.send().await {
        Ok(r) => {
            let st = r.status(); let txt = r.text().await.unwrap_or_default();
            if st.is_success() { (true, "ok".into()) }
            else { (false, format!("{} {}", st.as_u16(), txt.chars().take(200).collect::<String>())) }
        }
        Err(e) => (false, format!("net: {e}")),
    }
}

// ── Manual cancel-all ───────────────────────────────────────────────────────
async fn manual_cancel_all(http: &reqwest::Client, auth: &Auth) -> bool {
    let path = "/cancel-all";
    let hdrs = hmac_headers(&auth.api_key, &auth.api_secret, &auth.passphrase, &auth.address, "DELETE", path, "");
    let mut req = http.delete(format!("{}{}", CLOB, path));
    for (k, v) in &hdrs { req = req.header(k.as_str(), v.as_str()); }
    match req.send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

// ── CLOB BBO task ───────────────────────────────────────────────────────────
async fn run_clob_bbo(
    shared: Arc<Shared>, yes_token: String,
    logfile: Arc<Mutex<std::fs::File>>, kill_low: f64, kill_high: f64,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let url = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
    loop {
        let (mut ws, _) = match tokio_tungstenite::connect_async(url).await {
            Ok(c) => c,
            Err(e) => { eprintln!("CLOB WS fail: {e}"); tokio::time::sleep(Duration::from_secs(2)).await; continue; }
        };
        let sub = serde_json::json!({"type":"market","assets_ids":[&yes_token],"custom_feature_enabled":true});
        let _ = ws.send(Message::Text(sub.to_string())).await;

        let mut bids: std::collections::HashMap<String,String> = std::collections::HashMap::new();
        let mut asks: std::collections::HashMap<String,String> = std::collections::HashMap::new();

        while let Some(Ok(msg)) = ws.next().await {
            if *shared.kill.lock().await { return; }
            let text = match msg {
                Message::Text(t) => t,
                Message::Ping(d) => { let _ = ws.send(Message::Pong(d)).await; continue; }
                _ => continue,
            };
            let d: serde_json::Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => continue };
            if !d.is_object() { continue; }
            let evt = d.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
            let aid = d.get("asset_id").and_then(|v| v.as_str()).unwrap_or("");

            if evt == "book" && aid == yes_token {
                bids.clear(); asks.clear();
                for b in d.get("bids").or(d.get("buys")).and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if let (Some(p), Some(s)) = (b.get("price").and_then(|v|v.as_str()), b.get("size").and_then(|v|v.as_str())) {
                        bids.insert(p.into(), s.into());
                    }
                }
                for a in d.get("asks").or(d.get("sells")).and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if let (Some(p), Some(s)) = (a.get("price").and_then(|v|v.as_str()), a.get("size").and_then(|v|v.as_str())) {
                        asks.insert(p.into(), s.into());
                    }
                }
            } else if evt == "price_change" {
                for pc in d.get("price_changes").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                    if pc.get("asset_id").and_then(|v|v.as_str()).unwrap_or("") != yes_token { continue; }
                    let p = pc.get("price").and_then(|v|v.as_str()).unwrap_or("0");
                    let sz = pc.get("size").and_then(|v|v.as_str()).unwrap_or("0");
                    let side = pc.get("side").and_then(|v|v.as_str()).unwrap_or("");
                    let bucket = if side == "BUY" { &mut bids } else { &mut asks };
                    if sz.parse::<f64>().unwrap_or(0.0) == 0.0 { bucket.remove(p); } else { bucket.insert(p.into(), sz.into()); }
                }
            }
            if !bids.is_empty() && !asks.is_empty() {
                let bb: f64 = bids.keys().filter_map(|p| p.parse().ok()).fold(0.0_f64, f64::max);
                let ba: f64 = asks.keys().filter_map(|p| p.parse().ok()).fold(f64::MAX, f64::min);
                let mid = (bb + ba) / 2.0;
                *shared.mid.lock().await = Some(mid);
                if mid < kill_low || mid > kill_high {
                    log(&mut *logfile.lock().await, &format!("KILL_SWITCH mid={:.4}", mid));
                    *shared.kill.lock().await = true;
                    return;
                }
            }
        }
        eprintln!("CLOB WS dropped, reconnecting...");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ── Main ────────────────────────────────────────────────────────────────────
#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    let mut logfile = std::fs::OpenOptions::new().create(true).append(true)
        .open("freq_test.log").expect("log file");
    log(&mut logfile, "═══ FREQ TEST v2 ═══");

    let pk = std::env::var("PRIVATE_KEY").expect("PRIVATE_KEY");
    let funder = std::env::var("FUNDER_ADDRESS").expect("FUNDER_ADDRESS");
    let api_key = std::env::var("API_KEY").expect("API_KEY");
    let api_secret = std::env::var("API_SECRET").expect("API_SECRET");
    let passphrase = std::env::var("PASSPHRASE")
        .or_else(|_| std::env::var("API_PASSPHRASE")).expect("PASSPHRASE");
    let max_cycles: usize = std::env::var("MAX_CYCLES").ok().and_then(|v| v.parse().ok()).unwrap_or(100);
    let kill_low: f64 = std::env::var("KILL_LOW").ok().and_then(|v| v.parse().ok()).unwrap_or(0.20);
    let kill_high: f64 = std::env::var("KILL_HIGH").ok().and_then(|v| v.parse().ok()).unwrap_or(0.80);
    let throttle_ms: u64 = std::env::var("THROTTLE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);

    let signer = LocalSigner::from_str(&pk).expect("bad pk").with_chain_id(Some(POLYGON));
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

    let auth = Auth {
        api_key, api_secret, passphrase,
        address: format!("0x{}", hex::encode(signer.address())),
    };
    log(&mut logfile, &format!("Auth address={}", auth.address));

    let http = reqwest::Client::builder().use_rustls_tls().http2_adaptive_window(true)
        .pool_max_idle_per_host(4).build().unwrap();

    // Fetch window tokens
    let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let wts = now_s - (now_s % 300);
    let window_end_ms = (wts + 300) * 1000;
    let slug = format!("btc-updown-5m-{}", wts);
    let gamma: serde_json::Value = http.get(format!("https://gamma-api.polymarket.com/events?slug={}&closed=false", slug))
        .header("User-Agent", "Mozilla/5.0").send().await.unwrap().json().await.unwrap();
    let tids: Vec<String> = serde_json::from_str(gamma[0]["markets"][0]["clobTokenIds"].as_str().unwrap()).unwrap();
    let token_yes = tids[0].clone();
    let remaining = 300 - (now_s - wts);
    log(&mut logfile, &format!("Window {} | ...{} | {}s left | throttle={}ms", wts, &token_yes[token_yes.len()-8..], remaining, throttle_ms));

    // Warm SDK cache
    let price = Decimal::from_str("0.01").unwrap();
    let size = Decimal::from(5i64);
    let _ = sdk.limit_order().token_id(&token_yes).size(size).price(price).side(Side::Buy).build().await;
    log(&mut logfile, "Cache warm");

    // BBO task
    let shared = Arc::new(Shared { mid: Mutex::new(None), kill: Mutex::new(false) });
    let logfile = Arc::new(Mutex::new(logfile));
    let bbo_handle = {
        let s = shared.clone(); let t = token_yes.clone(); let lf = logfile.clone();
        tokio::spawn(async move { run_clob_bbo(s, t, lf, kill_low, kill_high).await; })
    };

    log(&mut *logfile.lock().await, "Waiting for BBO...");
    loop { if shared.mid.lock().await.is_some() { break; } tokio::time::sleep(Duration::from_millis(50)).await; }
    log(&mut *logfile.lock().await, &format!("BBO ready mid={:.4}", shared.mid.lock().await.unwrap()));

    // Cycle loop
    let mut place_rtts: Vec<f64> = Vec::new();
    let mut cancel_rtts: Vec<f64> = Vec::new();
    let mut cycle_times: Vec<f64> = Vec::new();
    let mut lockups = 0usize;
    let mut rate_limits = 0usize;
    let mut place_fails = 0usize;
    let mut cancel_fails = 0usize;
    let mut cycles_done = 0usize;

    log(&mut *logfile.lock().await, &format!("{:>4} {:>8} {:>8} {:>8} {:>8}  {}", "#", "pl_ms", "cn_ms", "cy_ms", "mid", "status"));

    for cycle in 1..=max_cycles {
        let cyc_start = Instant::now();
        if *shared.kill.lock().await { log(&mut *logfile.lock().await, "Kill switch"); break; }
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        if now_ms >= window_end_ms { log(&mut *logfile.lock().await, "Window ended"); break; }

        // PLACE
        let t0 = Instant::now();
        let (oid, ok, reason) = manual_place(&http, &auth, &sdk, &signer, &domain, &token_yes, price, size).await;
        let pl_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if !ok {
            place_fails += 1;
            if reason.contains("balance") || reason.contains("insufficient") { lockups += 1; }
            if reason.contains("429") || reason.contains("rate") { rate_limits += 1; }
        }

        // CANCEL
        let mut cn_ms = 0.0;
        let mut cn_reason = "skip".to_string();
        if ok && !oid.is_empty() {
            let t1 = Instant::now();
            let (cn_ok, r) = manual_cancel(&http, &auth, &oid).await;
            cn_ms = t1.elapsed().as_secs_f64() * 1000.0;
            cn_reason = r;
            if cn_ok { cancel_rtts.push(cn_ms); } else { cancel_fails += 1; }
        }

        // THROTTLE
        if throttle_ms > 0 { tokio::time::sleep(Duration::from_millis(throttle_ms)).await; }

        let cy_ms = cyc_start.elapsed().as_secs_f64() * 1000.0;
        if ok { place_rtts.push(pl_ms); }
        cycle_times.push(cy_ms);
        cycles_done = cycle;
        let mid = shared.mid.lock().await.unwrap_or(0.0);

        let status = if !ok { format!("PLACE_FAIL: {}", &reason[..reason.len().min(60)]) }
            else if cn_reason != "ok" { format!("CANCEL_FAIL: {}", &cn_reason[..cn_reason.len().min(60)]) }
            else { "ok".into() };

        log(&mut *logfile.lock().await, &format!("{:>4} {:>8.1} {:>8.1} {:>8.1} {:>8.4}  {}", cycle, pl_ms, cn_ms, cy_ms, mid, status));
    }

    // Cancel-all
    log(&mut *logfile.lock().await, "Cancel-all...");
    let ok = manual_cancel_all(&http, &auth).await;
    log(&mut *logfile.lock().await, &format!("Cancel-all {}", if ok {"OK"} else {"FAIL"}));

    *shared.kill.lock().await = true;
    let _ = bbo_handle.await;

    // Summary
    let mut f = logfile.lock().await;
    log(&mut f, "");
    log(&mut f, "═══ SUMMARY ═══");
    log(&mut f, &format!("Cycles: {}  Place fails: {} (lockup={} rate={})", cycles_done, place_fails, lockups, rate_limits));
    log(&mut f, &format!("Cancel fails: {}", cancel_fails));
    if !place_rtts.is_empty() {
        let mut s = place_rtts.clone(); s.sort_by(|a,b| a.partial_cmp(b).unwrap());
        log(&mut f, &format!("Place RTT:  min={:.1} avg={:.1} p95={:.1} max={:.1}", s[0], s.iter().sum::<f64>()/s.len() as f64, s[(s.len() as f64*0.95) as usize], s.last().unwrap()));
    }
    if !cancel_rtts.is_empty() {
        let mut s = cancel_rtts.clone(); s.sort_by(|a,b| a.partial_cmp(b).unwrap());
        log(&mut f, &format!("Cancel RTT: min={:.1} avg={:.1} p95={:.1} max={:.1}", s[0], s.iter().sum::<f64>()/s.len() as f64, s[(s.len() as f64*0.95) as usize], s.last().unwrap()));
    }
    if !cycle_times.is_empty() {
        let mut s = cycle_times.clone(); s.sort_by(|a,b| a.partial_cmp(b).unwrap());
        let total = s.iter().sum::<f64>() / 1000.0;
        log(&mut f, &format!("Cycle:      min={:.1} avg={:.1} p95={:.1} max={:.1}  throughput={:.1}/s", s[0], s.iter().sum::<f64>()/s.len() as f64, s[(s.len() as f64*0.95) as usize], s.last().unwrap(), cycles_done as f64/total));
    }
    log(&mut f, "═══ DONE ═══");
}
