# Before LIVE can be enabled

LIVE and CONFIRM exist in code and are covered by unit tests with mocked
backends, but **no transaction has ever been signed or sent by this system**.
Every item below must be true, in order. Do not skip ahead because a PAPER
session showed a few positive samples.

## A. Evidence that there is an edge (PAPER)

1. Several PAPER soak sessions (different times of day, ≥ 24 h total) recorded
   with a **representative simulation taker**: set `wallet.pubkey` to the real bot
   wallet and remove `paper.simulation_taker`, so ATA/rent/wSOL state matches
   production.
2. `mobius-searcher --report <id>` shows, per strategy: `net-positive` evaluations
   after **all** costs, `executable` > 0, and the **median sim − model net** close
   to 0 (the cost model is calibrated).
3. Executable opportunities survive realistic latency: compare quote age,
   simulation latency and `gross-positive episode` lifetimes against the time
   you need to land (sampling gap is the resolution limit — measure with a paid
   Jupiter plan / faster RPC before concluding anything).
4. Paper PnL still positive after applying a pessimistic landing rate (paper
   fills assume 100% landing; Jito auctions do not).

## B. Infrastructure

5. Paid Jupiter plan sized for the scan rate (Free = 1 rps shared by all
   requests in the organisation).
6. Private RPC with `simulateTransaction` capacity (`SOLANA_RPC_URL` /
   `SOLANA_WS_URL`); public mainnet RPC rate-limits simulation.
7. Optional Jito UUID (`JITO_UUID`) if the default 1 rps/IP/region is not enough;
   pick the block-engine region closest to you.

## C. Wallet

8. Dedicated **hot wallet** holding only what the strategy needs (+ fees).
   Never the main wallet.
9. If the dedicated Jupiter hot wallet exposes a 64-byte base58 **private-key**
   export, save it (not the recovery phrase) to a file. A Solana CLI 64-byte
   JSON keypair is also accepted. If the wallet only exposes a recovery phrase,
   create a separate Solana CLI hot wallet and fund it instead of storing the
   phrase in the bot. Run `chmod 600 /absolute/path/to/jupiter-hot-wallet.key`,
   set that absolute path in `wallet.keypair_path`, and set `wallet.pubkey` to
   the same public key (startup verifies they match). Keep the file outside
   this repository and never paste the private key into chat, config, or logs.
   Verify the file offline before enabling trading:
   `mobius-searcher --check-wallet /absolute/path/to/jupiter-hot-wallet.key`.
10. Hold some **USDC** as well as SOL: a leg's input is fixed at the previous
    leg's quote, and when that leg delivers a little less the wallet's USDC
    makes up the difference (`INVENTORY_LOW` otherwise). `--doctor` shows how
    many worst-case differences the USDC covers; aim for well over 20. Keep
    **no standing wSOL** balance (Jupiter's unwrap closes the wSOL account).
    Token accounts a trade creates are deposits (capital, capped by
    `profit.max_new_deposit_lamports`), not costs.
11. `risk.min_wallet_sol_for_fees_lamports` ≥ a day of fees + tips.

## D. Limits (start tiny)

12. `max_trade_lamports` small (e.g. 0.05 SOL), `max_daily_loss_usd` small,
    `max_consecutive_failures` ≤ 3, `max_open_executions = 1`.
13. `max_jito_tip_lamports` and tip policy reviewed against the tip floor.
14. `protect_min_out = true` (on-chain min-out ≥ input + costs + min profit).

## E. Staged enablement

15. Run the **canary** first: `mobius-searcher --canary` sends one approved,
    loss-bounded SOL → USDC → SOL trade and reconciles it account by account.
    Do not go further unless its report says ALL LINES MATCH.
16. Run **CONFIRM** next: `mode = "confirm"`, `live_enabled = true`. Every
    trade is shown in the TUI banner and waits for `y`. Verify for each landed
    bundle: realized PnL vs expected, fees, tip, CU, landing latency.
17. Confirm the kill switch (`K`) stops new submissions immediately while the
    process, recording and UI keep running.
18. Only then `mode = "live"`, with the same small limits, watching the Risk and
    System pages.

## Known gaps to close before scaling up

- No on-chain assertion program (e.g. Lighthouse) for pre/post-state checks;
  atomicity relies on a single transaction + Jupiter min-out. Multi-tx bundle
  plans are refused for sending (`exact_simulation_required`).
- Multi-tx bundles cannot be simulated exactly without a Jito-Solana RPC
  (`simulateBundle`).
- Realized PnL in LIVE is the native SOL balance delta around the bundle; the
  full SOL + USDC reconciliation runs for the canary, and the wallet ledger
  (`--report`) splits the USD change into trades, deposits and price moves.
- No automatic resend: a timed-out bundle is recorded as expired.
