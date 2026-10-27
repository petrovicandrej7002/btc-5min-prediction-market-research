# Notebooks

Five notebooks, in the order of the main README: first the market (01, 02), then how BTC moves (03, 04), then the
distribution a fair price needs (05). The code was written by an AI assistant from my instructions (see
[How this was made](../README.md#how-this-was-made)). The data is not in this repo, but every notebook keeps the printed
outputs and charts of my own run, so it can be read without re-running it. Each ends with "Reading the results". Figures
are saved to [`../figures/`](../figures/).

| # | Question | Main numbers | Needs under `data/` |
|---|---|---|---|
| [01](01_polymarket_reaction_to_binance.ipynb) | How does Polymarket's price follow Binance, and who makes money? | 61% of a 3¢+ fair-price move in the mid after 0.5 s, 83% after 1 s; buying at the saved quote pays 3.57¢ a share at once, 0.85¢ at 500 ms, nothing at 700 ms; fast takers +$65k a day, takers with no move −$278k, makers +$118k before rebates, Polymarket $176k in fees; stress tests | `clob_bbo/`, `clob_trades/`, `binance_spot/`, `windows.csv`, `clifford/` from 05 |
| [02](02_polymarket_orderbook_calibration.ipynb) | How well calibrated is Polymarket's own price? | Mid Brier 0.1775, calibration error 0.97¢, deviations not distinguishable from noise; 0.2201 early, 0.1287 late; spread wider than one tick 10.9% of the time; the price gets steeper as time runs out (`k` 0.018 to 0.100) | `clob_bbo/`, `binance_spot/`, `windows.csv` |
| [03](03_return_distributions_by_regime.ipynb) | How fat are the tails of 10, 100 and 300 second BTC moves, by volatility regime? | Tail ratio `(p99 − p1)/(p90 − p10)` 2.26 to 3.41 (Normal = 1.82); spread ×6.9 and ×5.2 from 10 s to 300 s vs ×5.5 for a random walk | `binance_spot/` |
| [04](04_serial_correlation_and_volatility_window.ipynb) | How long does momentum last, and which trailing volatility window separates regimes best? | Autocorrelation +0.18 at 100 ms, about 0 from 7.5 s; same-sign rate 60% at 100 ms, at most 53.3% from 3 s on; 60 s window: Q5/Q1 3.15× | `binance_spot/` |
| [05](05_conditional_cdf_grid_and_validation.ipynb) | Can a lookup table of past moves, by volatility and time left, give calibrated probabilities? | Calibration error 0.7 points at 60 s left (2.8 for a Normal with the same spread); correct calls 55% to 98% with the margin | `binance_monthly_trades/`, `binance_spot/` |

`clifford/` is the folder of the lookup table (my working name for it): notebook 05 writes it and notebook 01 reads it.
Which data each notebook uses is listed in the [main README](../README.md#data-behind-each-notebook).
