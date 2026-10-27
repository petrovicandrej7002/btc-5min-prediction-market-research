#!/usr/bin/env python3
"""
Data collector for Polymarket's 5-minute BTC Up/Down markets.

Records four things, all from public sources, stamped with this machine's clock:
  data/binance_spot/   every Binance BTCUSDT trade        (public trade WebSocket)
  data/clob_bbo/       the Up token's best bid and ask     (Polymarket's public market WebSocket, sampled every 100 ms)
  data/clob_trades/    every trade on the Up and Down tokens (same WebSocket: price, size, taker side, fee field)
  data/windows.csv     one row per market: strike, close and outcome (Polymarket's public APIs)

Trades and quotes go to hourly CSV files that are gzipped when the hour ends.
Because both feeds are stamped with the same clock (local_ms), they can be compared to the millisecond.

Run:  python capture/collect.py          (Ctrl+C to stop)
This is a cleaned version of the collector I ran in March and April 2026: other exchanges and other data sources are
left out, the parts the notebooks use are unchanged.
"""
import asyncio, csv, gzip, json, shutil, signal, time
from datetime import datetime, timezone
from pathlib import Path

import aiohttp
import websockets

DATA_DIR   = Path("data")
WINDOW_SEC = 300
BBO_EVERY  = 0.1   # seconds between best bid/ask samples

WS_BINANCE   = "wss://stream.binance.com:9443/ws/btcusdt@trade"
WS_CLOB      = "wss://ws-subscriptions-clob.polymarket.com/ws/market"
GAMMA_API    = "https://gamma-api.polymarket.com/events"
CRYPTO_PRICE = "https://polymarket.com/api/crypto/crypto-price"

STOP = asyncio.Event()


def now_ms():
    return int(time.time() * 1000)


def log(msg):
    print(f"[{datetime.now(timezone.utc):%H:%M:%S}] {msg}", flush=True)


def window_start(t=None):
    t = int(time.time()) if t is None else t
    return t - t % WINDOW_SEC


def iso(ts):
    return datetime.fromtimestamp(ts, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# ------------------------------------------------------------------------------------------------
#  Hourly CSV writer: buffers rows, writes every few seconds, gzips each finished hour
# ------------------------------------------------------------------------------------------------
class HourlyCSV:
    def __init__(self, subdir, header):
        self.dir = DATA_DIR / subdir
        self.dir.mkdir(parents=True, exist_ok=True)
        self.header, self.buf, self.hour, self.fh = header, [], None, None

    def write(self, row):
        self.buf.append(row)

    def flush(self):
        hour = datetime.now(timezone.utc).strftime("%Y-%m-%d_%H")
        if hour != self.hour:
            self._rotate(hour)
        csv.writer(self.fh).writerows(self.buf)
        self.fh.flush()
        self.buf.clear()

    def _rotate(self, hour):
        if self.fh:
            self.fh.close()
            done = Path(self.fh.name)
            with open(done, "rb") as src, gzip.open(f"{done}.gz", "wb") as dst:
                shutil.copyfileobj(src, dst)
            done.unlink()
        path = self.dir / f"{hour}.csv"
        new = not path.exists()
        self.fh, self.hour = open(path, "a", newline=""), hour
        if new:
            csv.writer(self.fh).writerow(self.header)

    def close(self):
        self.flush()
        self.fh.close()


# ------------------------------------------------------------------------------------------------
#  Binance trades
# ------------------------------------------------------------------------------------------------
async def binance_trades(out):
    backoff = 1
    while not STOP.is_set():
        try:
            async with websockets.connect(WS_BINANCE, ping_interval=20) as ws:
                log("[BINANCE] connected")
                backoff = 1
                async for raw in ws:
                    d = json.loads(raw)
                    out.write([now_ms(), d["T"], d["p"], d["q"], int(d["m"])])
        except Exception as e:
            log(f"[BINANCE] disconnected ({e}), retrying in {backoff}s")
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 30)


# ------------------------------------------------------------------------------------------------
#  Polymarket order book for the current market's Up token
# ------------------------------------------------------------------------------------------------
class Book:
    """Local copy of the Up token's book, rebuilt from snapshots and updates."""
    def __init__(self):
        self.token, self.bids, self.asks, self.server_ms = "", {}, {}, "0"

    def reset(self, token):
        self.token, self.bids, self.asks = token, {}, {}

    def snapshot(self, d):
        self.bids = {b["price"]: b["size"] for b in d.get("bids", [])}
        self.asks = {a["price"]: a["size"] for a in d.get("asks", [])}
        self.server_ms = d.get("timestamp", "0")

    def update(self, change, server_ms):
        side = self.bids if change["side"] == "BUY" else self.asks
        if float(change["size"]) == 0:
            side.pop(change["price"], None)
        else:
            side[change["price"]] = change["size"]
        self.server_ms = server_ms

    def best(self):
        bid = max(self.bids, key=float) if self.bids else "0"
        ask = min(self.asks, key=float) if self.asks else "1"
        return bid, self.bids.get(bid, "0"), ask, self.asks.get(ask, "0")


async def polymarket_book(book, tokens_q, fills):
    tokens, backoff = [], 1
    while not STOP.is_set():
        try:
            async with websockets.connect(WS_CLOB, ping_interval=20, max_size=10_000_000) as ws:
                log("[CLOB] connected")
                backoff = 1
                while not tokens_q.empty():
                    tokens = tokens_q.get_nowait()
                if tokens:
                    book.reset(tokens[0])
                    await ws.send(json.dumps({"type": "market", "assets_ids": tokens}))
                while not STOP.is_set():
                    # a new market starts every 5 minutes: switch the subscription to its tokens
                    if not tokens_q.empty():
                        new = tokens_q.get_nowait()
                        if tokens:
                            await ws.send(json.dumps({"assets_ids": tokens, "operation": "unsubscribe"}))
                        tokens = new
                        book.reset(tokens[0])
                        await ws.send(json.dumps({"assets_ids": tokens, "operation": "subscribe"}))
                    try:
                        raw = await asyncio.wait_for(ws.recv(), timeout=1)
                    except asyncio.TimeoutError:
                        continue
                    msgs = json.loads(raw)
                    for d in msgs if isinstance(msgs, list) else [msgs]:
                        event = d.get("event_type")
                        if event == "book" and d.get("asset_id") == book.token:
                            book.snapshot(d)
                        elif event == "price_change":
                            for change in d.get("price_changes", []):
                                if change.get("asset_id") == book.token:
                                    book.update(change, d.get("timestamp", "0"))
                        elif event == "last_trade_price" and d.get("asset_id") in tokens:
                            fills.write([now_ms(), d.get("timestamp", "0"), d.get("price"), d.get("size"),
                                         d.get("side"), d.get("fee_rate_bps"), d["asset_id"]])
        except Exception as e:
            log(f"[CLOB] disconnected ({e}), retrying in {backoff}s")
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 30)


async def sample_bbo(book, out):
    """Write the Up token's best bid and ask every 100 ms."""
    while not STOP.is_set():
        if book.bids or book.asks:
            out.write([now_ms(), book.server_ms, *book.best(), book.token])
        await asyncio.sleep(BBO_EVERY)


# ------------------------------------------------------------------------------------------------
#  Markets: find each new market's tokens, and record strike and outcome when it ends
# ------------------------------------------------------------------------------------------------
async def get_json(session, url, params):
    try:
        async with session.get(url, params=params, timeout=aiohttp.ClientTimeout(total=15)) as r:
            return await r.json() if r.status == 200 else None
    except Exception:
        return None


async def market_tokens(session, ts):
    """Up and Down token ids and condition id of the market that opens at ts."""
    for _ in range(10):
        events = await get_json(session, GAMMA_API, {"slug": f"btc-updown-5m-{ts}"})
        if events:
            m = events[0]["markets"][0]
            ids = m["clobTokenIds"]
            return (json.loads(ids) if isinstance(ids, str) else ids), m.get("conditionId", "")
        await asyncio.sleep(3)
    return None, ""


async def market_prices(session, ts):
    """Strike (open) and, once the market has ended, close price as Polymarket publishes them."""
    return await get_json(session, CRYPTO_PRICE, {
        "symbol": "BTC", "variant": "fiveminute",
        "eventStartTime": iso(ts), "endDate": iso(ts + WINDOW_SEC)})


async def markets(tokens_q):
    path = DATA_DIR / "windows.csv"
    if not path.exists():
        with open(path, "w", newline="") as f:
            csv.writer(f).writerow(["window_ts", "slug", "condition_id", "open_price",
                                    "close_price", "outcome", "resolved_via"])

    async with aiohttp.ClientSession() as session:
        ts = window_start()
        tokens, cond = await market_tokens(session, ts)
        if tokens:
            await tokens_q.put(tokens)
        while not STOP.is_set():
            # sleep until the next 5-minute boundary
            try:
                await asyncio.wait_for(STOP.wait(), timeout=ts + WINDOW_SEC - time.time())
                break
            except asyncio.TimeoutError:
                pass
            prev_ts, prev_cond = ts, cond
            ts = window_start()
            tokens, cond = await market_tokens(session, ts)
            if tokens:
                await tokens_q.put(tokens)

            # record the market that just ended (Polymarket publishes the close a little later)
            row = [prev_ts, f"btc-updown-5m-{prev_ts}", prev_cond, None, None, "UNKNOWN", "failed"]
            for _ in range(6):
                p = await market_prices(session, prev_ts)
                if p and p.get("completed") and p.get("closePrice") is not None:
                    op, cl = p["openPrice"], p["closePrice"]
                    row[3:] = [op, cl, "UP" if cl >= op else "DOWN", "crypto-price"]
                    break
                await asyncio.sleep(15)
            with open(path, "a", newline="") as f:
                csv.writer(f).writerow(row)
            log(f"[MARKET] {row[1]}: {row[5]} ({row[3]} -> {row[4]})")


# ------------------------------------------------------------------------------------------------
async def flush_every(writers, sec=5):
    while not STOP.is_set():
        await asyncio.sleep(sec)
        for w in writers:
            w.flush()


async def main():
    DATA_DIR.mkdir(exist_ok=True)
    trades = HourlyCSV("binance_spot", ["local_ms", "exchange_ms", "price", "qty", "is_buyer_maker"])
    bbo    = HourlyCSV("clob_bbo", ["local_ms", "server_ms", "best_bid", "bid_size", "best_ask", "ask_size", "asset_id"])
    fills  = HourlyCSV("clob_trades", ["local_ms", "server_ms", "price", "size", "side", "fee_bps", "asset_id"])
    book, tokens_q = Book(), asyncio.Queue()

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, STOP.set)

    tasks = [asyncio.create_task(c) for c in (
        binance_trades(trades), polymarket_book(book, tokens_q, fills), sample_bbo(book, bbo),
        markets(tokens_q), flush_every([trades, bbo, fills]))]
    log(f"recording to {DATA_DIR.resolve()}, Ctrl+C to stop")
    await STOP.wait()
    for t in tasks:
        t.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    for w in (trades, bbo, fills):
        w.close()
    log("stopped")


if __name__ == "__main__":
    asyncio.run(main())
