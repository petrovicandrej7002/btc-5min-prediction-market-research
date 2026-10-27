# Order speed tests (Rust)

Two small programs I used to test how fast my server could change a quote on Polymarket. Polymarket's matching
engine is in London, and my server was in Ireland, the closest region where Polymarket is available, about 10 ms away
one way (8 to 15 ms). I wrote them with an AI coding assistant. They are tests, not the trading bot, which is not in
this repo.

## The idea

To cancel or replace an order you need its ID. Normally the ID comes back in the reply, so you wait a full round trip
(20 to 25 ms from Ireland) before you can touch the order again. That caps you at one new quote per round trip.

On Polymarket the ID of an order is its EIP-712 hash. The order includes a random salt that the client picks, so the
hash can be computed before the order is sent ([gap_test/src/main.rs](gap_test/src/main.rs), `prepare_order_async`):

```rust
let signed = sdk.sign(signer, order).await
    .map_err(|e| format!("sign: {e}"))?;

let hash = signed.order.eip712_signing_hash(domain);
let order_id = format!("0x{}", hex::encode(hash));
```

With the ID known in advance, the order is sent without waiting for the reply and the cancel follows a few
milliseconds later. Both go over the same HTTP/2 connection, so the order reaches Polymarket before its cancel. The
quote can then change every few milliseconds instead of once per round trip. The delay before news reaches London
stays the same; what changes is how often the quote can follow it.

![Same delay, more updates](../figures/00_quote_update_timing.png)

## What each test does

| Test | What it does | My results |
|---|---|---|
| [freq_test](freq_test/src/main.rs) | The usual way: place an order, wait for the reply, cancel it, wait again, repeat. | Place round trip 20 to 25 ms, cancel 17 to 22 ms, a full cycle 40 to 50 ms (about 20 a second). No rate-limit errors up to 48 cycles a second. |
| [gap_test](gap_test/src/main.rs) | Computes the ID, sends the order without waiting, then sends the cancel 10, 5, 3, 1 or 0 ms later. | Every cancel worked, 10 of 10 at each gap. 1 to 3 ms was cleanest; at 0 ms the replies became erratic (24 to 670 ms). |

At a 3 ms gap that is room for over 300 changes a second. Polymarket's rate limits cap how long a burst like that can
last, so the speed is for the moments right after a move, not for all day.

## Running them

They place real orders from your own Polymarket wallet: a post-only buy of 5 shares at $0.01 on the current BTC
5-minute market, far below the market so it does not fill. Each test stops if the market's mid leaves 0.20 to 0.80 and
cancels all its open orders at the end. Read the code before running it.

```
cp .env.example .env        # your own keys; .env is ignored by git
cargo run --release -p freq_test
cargo run --release -p gap_test
```

Run them from this folder so they find `.env`. Each writes a log (`freq_test.log`, `gap_test.log`) where it runs.
`Cargo.lock` pins crate versions that compile.
