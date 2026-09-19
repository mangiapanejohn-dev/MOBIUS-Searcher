# Security

## Reporting a vulnerability

Please report security problems privately through GitHub: **Security → Report a
vulnerability** on this repository. Do not open a public issue for anything
that could put someone's keys or funds at risk.

## How secrets are handled

- **No secrets in configuration files.** Config files hold only the *names* of
  environment variables (`api_key_env = "JUPITER_API_KEY"`). Values come from
  the process environment, `~/.config/mobius/.env` or `./.env`.
- **Nothing personal in the repository.** Your settings live in
  `~/.config/mobius/config.toml`; `.env`, databases and `*.keypair.json` are
  git-ignored.
- **The private key is a file**, `wallet.keypair_path`, which must be
  `chmod 600` — MØBIUS refuses a key file that others can read. On Windows it
  cannot check the file's permissions (NTFS ACLs apply): keep the key in your
  user profile, e.g. `%USERPROFILE%\.config\mobius\wallets\`, where only your
  account can read it. A key in the environment never enables sending.
- **PAPER never reads a private key.** It needs at most a public key.
- API keys and RPC URLs (which can embed keys) are redacted in debug output
  (`ApiKey(***)`, `RpcClient(url=<redacted>)`). `--doctor` and
  `--print-config` show secret *names* only, never values.
- The recording database stores market data and the bot's decisions. It never
  stores API keys or private keys; it does store the wallet's public address.

## Gates in front of real transactions

1. `execution.live_enabled = true` **and** a `wallet.keypair_path` — without
   both, CONFIRM and LIVE refuse to start.
2. The mode must be `confirm` or `live` (`--mode` or `general.mode`).
3. Every transaction is assembled and simulated first. A simulation error, or a
   result that is not profitable after all costs, means it is never sent.
4. The final swap carries an on-chain minimum output (`protect_min_out`), so a
   price move makes the transaction revert instead of realizing a loss.
5. The risk engine checks size, share of equity, daily loss, fee reserve,
   quote and simulation age and consecutive failures.
6. The kill switch (`K`) stops new submissions immediately; releasing it needs
   a keyboard confirmation.
7. In CONFIRM, every transaction waits for `y`.

## Operating safely

- Use a **dedicated hot wallet** holding only what the strategy needs. Never
  your main wallet.
- Start in PAPER, then CONFIRM, with small limits. Follow
  [docs/LIVE_CHECKLIST.md](docs/LIVE_CHECKLIST.md).
- Keep `~/.config/mobius/` readable only by you.

This software is provided as is, without warranty. Trading can lose money;
nothing here is financial advice.
