//! Configuration. Secrets never live here: only the *names* of environment
//! variables and the path of a keypair file. `summary()` is safe to log.
//!
//! Layers, later wins (see [`load_layered`]):
//!   built-in defaults → `config/mobius.toml` (repository, shared, no personal
//!   data) → user file (`~/.config/mobius/config.toml`, or `--config` /
//!   `MOBIUS_CONFIG`) → command-line flags.
//! Tables merge key by key (built-in defaults included, so `[venues.okx]` in
//! your file changes only the keys it names); arrays (strategies, pools, …)
//! replace as a whole.
//!
//! Markets are venues: `[venues.<name>] kind = "…"`, each with its own
//! endpoints, credential variable names and instruments. The Solana stack
//! ([rpc], [jupiter], [jito], [feeds], [wallet], strategies) still lives in
//! top-level sections; it moves under `[venues.solana]` later.
//! Secrets come from the environment: the process, then
//! `~/.config/mobius/.env`, then `./.env` (the first one that sets a name wins).

use crate::address::Address;
use crate::costs::CostParams;
use crate::model::{Mode, SlippageSpec};
use crate::profit::ProfitGuards;
use crate::token::{Token, TokenRegistry};
use crate::units::{Ppm, UsdMicros, parse_decimal};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub general: GeneralConfig,
    pub jupiter: JupiterConfig,
    pub rpc: RpcConfig,
    pub jito: JitoConfig,
    pub wallet: WalletConfig,
    pub paper: PaperConfig,
    pub profit: ProfitConfig,
    pub risk: RiskConfig,
    pub execution: ExecutionConfig,
    pub strategies: StrategiesConfig,
    pub tokens: Vec<Token>,
    pub ui: UiConfig,
    pub feeds: FeedsConfig,
    pub scheduler: SchedulerConfig,
    pub network: NetworkConfig,
    pub storage: StorageConfig,
    /// `--research`: measurements only (quotes, prices), nothing is signed.
    pub research: ResearchConfig,
    /// `--canary`: one real trade through the LIVE path, loss-bounded.
    pub canary: CanaryConfig,
    /// Venues other than the Solana stack, by a name you choose.
    pub venues: BTreeMap<String, VenueConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: Default::default(),
            jupiter: Default::default(),
            rpc: Default::default(),
            jito: Default::default(),
            wallet: Default::default(),
            paper: Default::default(),
            profit: Default::default(),
            risk: Default::default(),
            execution: Default::default(),
            strategies: Default::default(),
            tokens: Default::default(),
            ui: Default::default(),
            feeds: Default::default(),
            scheduler: Default::default(),
            network: Default::default(),
            storage: Default::default(),
            research: Default::default(),
            canary: Default::default(),
            venues: BTreeMap::from([
                ("okx".to_string(), VenueConfig::okx()),
                ("binance".to_string(), VenueConfig::binance()),
                (
                    "ethereum".to_string(),
                    VenueConfig::evm(
                        1,
                        "https://ethereum-rpc.publicnode.com",
                        "ETHEREUM_RPC_URL",
                        "0x61fFE014bA17989E743c5F6cB21bF9697530B21e",
                        "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45",
                        "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640",
                    ),
                ),
                (
                    "base".to_string(),
                    VenueConfig::evm(
                        8453,
                        "https://base-rpc.publicnode.com",
                        "BASE_RPC_URL",
                        "0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a",
                        "0x2626664c2603336E57B271c5C0b26F421741e481",
                        "0xd0b53D9277642d899DF5C87A3966A349A798F224",
                    ),
                ),
                (
                    "arbitrum".to_string(),
                    VenueConfig::evm(
                        42161,
                        "https://arbitrum-one-rpc.publicnode.com",
                        "ARBITRUM_RPC_URL",
                        "0x61fFE014bA17989E743c5F6cB21bF9697530B21e",
                        "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45",
                        "0xC6962004f452bE9203591991D15f6b388e09E8D0",
                    ),
                ),
            ]),
        }
    }
}

/// Connector a venue uses. Only kinds with a connector in this build are
/// accepted; a new exchange or chain is a new variant plus its connector.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VenueKind {
    /// OKX v5 API. Public market data today (Markets page); trading next.
    Okx,
    /// Binance spot. Market data from the public mirror by default; signed
    /// requests go to `rest_url` (or the spot testnet with `demo = true`).
    /// Binance refuses some locations (HTTP 451).
    Binance,
    /// An EVM chain over JSON-RPC (`rest_url`): Uniswap v3 pool state and
    /// exact quotes from its QuoterV2. Read-only for now.
    Evm,
}

/// One market venue. Credentials are variable *names*; values come from the
/// environment / `.env` like every other secret.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueConfig {
    pub kind: VenueKind,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// REST base URL (e.g. `https://www.okx.com`; regional hosts work too).
    pub rest_url: String,
    /// Instruments in the venue's own notation (OKX: `SOL-USDT`), first = default.
    #[serde(default)]
    pub markets: Vec<String>,
    /// Extra instruments shown as tickers only.
    #[serde(default)]
    pub watchlist: Vec<String>,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub secret_env: String,
    #[serde(default)]
    pub passphrase_env: String,
    /// Place orders here. Refused until the venue's trading connector exists.
    #[serde(default)]
    pub trading: bool,
    /// Orders go to the venue's demo environment (simulated funds; OKX: demo
    /// API key + `x-simulated-trading: 1`). A real-account order needs
    /// `demo = false`, `trading = true`, a sending mode and
    /// `execution.live_enabled = true`.
    #[serde(default = "yes")]
    pub demo: bool,
    /// Taker fee assumed for paper fills and cost estimates, bps.
    #[serde(default = "default_taker_fee_bps")]
    pub taker_fee_bps: u32,
    /// EVM: chain id the RPC must report (1 Ethereum, 8453 Base, 42161 Arbitrum).
    #[serde(default)]
    pub chain_id: Option<u64>,
    /// EVM: env var that overrides `rest_url` (keyed RPC URLs belong in the environment).
    #[serde(default)]
    pub rpc_url_env: String,
    /// EVM: Uniswap v3 QuoterV2 address.
    #[serde(default)]
    pub quoter: String,
    /// EVM: Uniswap v3 pool addresses; tokens, decimals and fee are read on chain.
    #[serde(default)]
    pub pools: Vec<String>,
    /// EVM: Uniswap SwapRouter02 address (swaps are sent here).
    #[serde(default)]
    pub router: String,
}

fn default_taker_fee_bps() -> u32 {
    10
}

fn yes() -> bool {
    true
}

impl VenueConfig {
    pub fn okx() -> Self {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect();
        Self {
            kind: VenueKind::Okx,
            enabled: true,
            rest_url: "https://www.okx.com".into(),
            markets: v(&["SOL-USDT", "SOL-USDC", "SOL-USD"]),
            watchlist: v(&["BTC-USDT", "ETH-USDT", "SOL-USDT", "XRP-USDT", "JUP-USDT", "USDC-USDT"]),
            api_key_env: "OKX_API_KEY".into(),
            secret_env: "OKX_API_SECRET".into(),
            passphrase_env: "OKX_API_PASSPHRASE".into(),
            trading: false,
            demo: true,
            taker_fee_bps: default_taker_fee_bps(),
            chain_id: None,
            rpc_url_env: String::new(),
            quoter: String::new(),
            pools: Vec::new(),
            router: String::new(),
        }
    }

    /// Binance spot market data from the public mirror. Disabled by default.
    pub fn binance() -> Self {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect();
        Self {
            kind: VenueKind::Binance,
            enabled: false,
            rest_url: "https://data-api.binance.vision".into(),
            markets: v(&["SOLUSDT", "SOLUSDC"]),
            watchlist: v(&["BTCUSDT", "ETHUSDT", "SOLUSDT"]),
            api_key_env: "BINANCE_API_KEY".into(),
            secret_env: "BINANCE_API_SECRET".into(),
            passphrase_env: String::new(),
            ..Self::okx()
        }
    }

    /// Uniswap v3 WETH/USDC 0.05 % on an EVM chain (addresses checked on
    /// chain 2026-09-19 against the official deployments). Disabled by default.
    pub fn evm(chain_id: u64, rpc: &str, rpc_env: &str, quoter: &str, router: &str, pool: &str) -> Self {
        Self {
            kind: VenueKind::Evm,
            enabled: false,
            rest_url: rpc.into(),
            markets: Vec::new(),
            watchlist: Vec::new(),
            api_key_env: String::new(),
            secret_env: String::new(),
            passphrase_env: String::new(),
            trading: false,
            demo: true,
            // Uniswap's pool fee is inside the quote
            taker_fee_bps: 0,
            chain_id: Some(chain_id),
            rpc_url_env: rpc_env.into(),
            quoter: quoter.into(),
            pools: vec![pool.into()],
            router: router.into(),
        }
    }

    /// The JSON-RPC / REST URL after the `rpc_url_env` override.
    pub fn resolved_url(&self) -> String {
        if self.rpc_url_env.is_empty() { self.rest_url.clone() } else { env_or(&self.rpc_url_env, &self.rest_url) }
    }

    /// Credential variable names that are configured (non-empty).
    pub fn credential_envs(&self) -> Vec<&str> {
        [&self.api_key_env, &self.secret_env, &self.passphrase_env]
            .into_iter()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GeneralConfig {
    pub mode: Mode,
    /// Recording database and reports. Empty (default): the per-user data
    /// directory, see [`user_data_dir`]. A relative path is relative to where
    /// the program is started.
    pub data_dir: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self { mode: Mode::Paper, data_dir: String::new() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JupiterConfig {
    pub base_url: String,
    /// Name of the env var holding the API key (the key itself is never in config).
    pub api_key_env: String,
    /// Requests/second for the shared Swap/Price/Tokens bucket.
    pub general_rps: f64,
    pub general_burst: u32,
    /// Buckets of Jupiter's own submission endpoints (`tx.jup.ag`, `/swap/v2/execute`).
    /// Accepted for plan sizing; this system submits through Jito and never calls them.
    pub submit_rps: f64,
    pub execute_rps: f64,
    pub timeout_ms: u64,
    /// `"rtse"` or an integer bps value as a string (e.g. `"30"`).
    pub slippage: String,
    /// `medium` | `high` | `veryHigh` | integer bps percentile.
    pub compute_unit_price_percentile: String,
    pub max_accounts: Option<u8>,
    pub blockhash_slots_to_expiry: u16,
    pub for_jito_bundle: bool,
    /// Pairs (e.g. `SOL/USDC`) liquid enough for `mode=fast`. Others use normal routing.
    pub fast_mode_pairs: Vec<String>,
    /// Reference price refresh (Price API v3), ms. 0 disables.
    pub price_refresh_ms: u64,
}

impl Default for JupiterConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.jup.ag".into(),
            api_key_env: "JUPITER_API_KEY".into(),
            general_rps: 0.9,
            general_burst: 2,
            submit_rps: 1.0,
            execute_rps: 1.0,
            timeout_ms: 4_000,
            slippage: "rtse".into(),
            compute_unit_price_percentile: "high".into(),
            max_accounts: None,
            blockhash_slots_to_expiry: 150,
            for_jito_bundle: true,
            fast_mode_pairs: vec!["SOL/USDC".into(), "USDC/SOL".into()],
            price_refresh_ms: 30_000,
        }
    }
}

impl JupiterConfig {
    /// Tolerance a leg may use, in bp: the fixed setting, else (`rtse`) the
    /// risk cap `max_slippage_bps` as the bound.
    pub fn tolerance_bps(&self, risk_cap_bps: u16) -> u32 {
        match self.slippage_spec() {
            Ok(SlippageSpec::Fixed(b)) => b as u32,
            _ => risk_cap_bps as u32,
        }
    }

    pub fn slippage_spec(&self) -> Result<SlippageSpec, String> {
        let s = self.slippage.trim();
        if s.eq_ignore_ascii_case("rtse") {
            return Ok(SlippageSpec::Rtse);
        }
        s.parse::<u16>()
            .ok()
            .filter(|b| *b <= 10_000)
            .map(SlippageSpec::Fixed)
            .ok_or_else(|| format!("jupiter.slippage must be \"rtse\" or 0..=10000 bps, got `{s}`"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RpcConfig {
    pub url: String,
    /// Env var that overrides `url` (provider URLs usually embed a key).
    pub url_env: String,
    pub ws_url: String,
    pub ws_url_env: String,
    pub rps: f64,
    pub burst: u32,
    pub simulate_rps: f64,
    pub timeout_ms: u64,
}

impl RpcConfig {
    /// `url`, unless the environment variable named by `url_env` is set
    /// (keyed provider URLs belong in the environment, not in a config file).
    pub fn resolved_url(&self) -> String {
        env_or(&self.url_env, &self.url)
    }

    pub fn resolved_ws_url(&self) -> String {
        env_or(&self.ws_url_env, &self.ws_url)
    }
}

fn env_or(var: &str, fallback: &str) -> String {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| fallback.to_string())
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            // keyless; measured 2026-09-19: account notifications current
            // (0–2 slots behind getSlot), while publicnode's WebSocket ran
            // ~30 slots (~12 s) behind its own HTTP endpoint
            url: "https://api.mainnet-beta.solana.com".into(),
            url_env: "SOLANA_RPC_URL".into(),
            ws_url: "wss://api.mainnet-beta.solana.com".into(),
            ws_url_env: "SOLANA_WS_URL".into(),
            rps: 3.0,
            burst: 3,
            simulate_rps: 0.5,
            timeout_ms: 5_000,
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TipPolicyKind {
    Fixed,
    #[default]
    Percentile,
    ProfitShare,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TipPolicyConfig {
    pub kind: TipPolicyKind,
    pub fixed_lamports: u64,
    /// `p25` | `p50` | `p75` | `p95` | `p99` | `ema50` of landed tips.
    pub percentile: String,
    /// Share of expected (pre-tip) net profit offered as tip, bps.
    pub profit_share_bps: u32,
    pub min_lamports: u64,
    pub max_lamports: u64,
}

impl Default for TipPolicyConfig {
    fn default() -> Self {
        Self {
            kind: TipPolicyKind::Percentile,
            fixed_lamports: 10_000,
            percentile: "p50".into(),
            profit_share_bps: 5_000,
            min_lamports: 1_000,
            max_lamports: 200_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JitoConfig {
    pub block_engine_url: String,
    pub tip_floor_url: String,
    /// Optional env var with an `x-jito-auth` UUID.
    pub uuid_env: String,
    pub rps: f64,
    pub tip_floor_refresh_ms: u64,
    pub tip_accounts_refresh_ms: u64,
    pub tip_policy: TipPolicyConfig,
    /// Add a read-only `jitodontfront…` account to our transaction.
    pub dont_front: bool,
}

impl Default for JitoConfig {
    fn default() -> Self {
        Self {
            block_engine_url: "https://mainnet.block-engine.jito.wtf".into(),
            tip_floor_url: "https://bundles.jito.wtf/api/v1/bundles/tip_floor".into(),
            uuid_env: "JITO_UUID".into(),
            rps: 1.0,
            tip_floor_refresh_ms: 20_000,
            tip_accounts_refresh_ms: 600_000,
            tip_policy: TipPolicyConfig::default(),
            dont_front: false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WalletConfig {
    /// Public key of the bot hot wallet. Enough for PAPER (simulation, balances).
    pub pubkey: Option<String>,
    /// Keypair file for CONFIRM/LIVE only. Accepts a Solana CLI JSON keypair or
    /// a 64-byte base58 wallet export. Must be 0600. Never logged.
    pub keypair_path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PaperConfig {
    /// Notional paper equity when no wallet balance is available.
    pub equity_lamports: u64,
    /// Simulation-only taker public key (overrides `wallet.pubkey` for
    /// `simulateTransaction`). Never signs anything.
    pub simulation_taker: Option<String>,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self { equity_lamports: 1_000_000_000, simulation_taker: None }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProfitConfig {
    pub min_profit_lamports: i64,
    pub min_profit_bps: i64,
    /// Decimal string, e.g. `"0.01"`.
    pub min_profit_usd: String,
    pub expected_slippage_share_bps: i64,
    pub safety_buffer_lamports: u64,
    pub safety_buffer_bps: i64,
    pub cu_margin_bps: i64,
    pub est_cu_per_leg: u32,
    pub max_cu_price_micro: u64,
    /// Tighten the final leg's min-out so the transaction reverts unless
    /// output ≥ input + costs + min profit (costs one extra `/build`).
    pub protect_min_out: bool,
    /// Largest rent deposit a trade may lock in accounts it leaves created
    /// (token accounts: 1,488,440 lamports each on 2026-09-22). Deposits are
    /// capital, not trade costs. 0 = never create accounts.
    pub max_new_deposit_lamports: u64,
}

impl Default for ProfitConfig {
    fn default() -> Self {
        Self {
            min_profit_lamports: 10_000,
            min_profit_bps: 5,
            min_profit_usd: "0.01".into(),
            expected_slippage_share_bps: 2_500,
            safety_buffer_lamports: 5_000,
            safety_buffer_bps: 1,
            cu_margin_bps: 2_000,
            est_cu_per_leg: 300_000,
            max_cu_price_micro: 200_000,
            protect_min_out: true,
            max_new_deposit_lamports: 3_000_000,
        }
    }
}

impl ProfitConfig {
    pub fn guards(&self) -> Result<ProfitGuards, String> {
        Ok(ProfitGuards {
            min_profit_lamports: self.min_profit_lamports,
            min_profit_edge: Ppm::from_bps(self.min_profit_bps),
            min_profit_usd: UsdMicros(crate::units::parse_signed_decimal(&self.min_profit_usd, 6)?),
        })
    }

    pub fn cost_params(&self) -> CostParams {
        CostParams {
            cu_margin: Ppm::from_bps(self.cu_margin_bps),
            est_cu_per_leg: self.est_cu_per_leg,
            max_cu_price_micro: self.max_cu_price_micro,
            expected_slippage_share: Ppm::from_bps(self.expected_slippage_share_bps),
            safety_buffer_lamports: self.safety_buffer_lamports,
            safety_buffer: Ppm::from_bps(self.safety_buffer_bps),
            // replaced by the chain's current value at engine start
            token_account_rent: crate::units::TOKEN_ACCOUNT_RENT_LAMPORTS,
            max_new_deposit: self.max_new_deposit_lamports,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RiskConfig {
    pub max_trade_lamports: u64,
    pub max_trade_pct_of_equity_bps: i64,
    /// Decimal USD string.
    pub max_daily_loss_usd: String,
    pub max_consecutive_failures: u32,
    pub max_slippage_bps: u16,
    pub max_quote_age_ms: u64,
    pub max_simulation_age_ms: u64,
    pub max_priority_fee_lamports: u64,
    pub max_jito_tip_lamports: u64,
    pub min_wallet_sol_for_fees_lamports: u64,
    pub max_open_executions: u32,
    /// Max slots between the latest observed slot and the quote/sim context.
    pub max_slot_lag: u64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_trade_lamports: 1_000_000_000,
            max_trade_pct_of_equity_bps: 10_000,
            max_daily_loss_usd: "5".into(),
            max_consecutive_failures: 5,
            max_slippage_bps: 100,
            max_quote_age_ms: 4_000,
            max_simulation_age_ms: 3_000,
            max_priority_fee_lamports: 100_000,
            max_jito_tip_lamports: 200_000,
            min_wallet_sol_for_fees_lamports: 20_000_000,
            max_open_executions: 1,
            max_slot_lag: 32,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExecutionConfig {
    /// Hard gate for CONFIRM and LIVE. Must be set explicitly; a key in the
    /// environment never enables sending on its own.
    pub live_enabled: bool,
    pub confirm_timeout_ms: u64,
    /// Compose all legs into one transaction when it fits (strongest atomicity).
    pub prefer_single_tx: bool,
    pub bundle_status_poll_ms: u64,
    pub bundle_timeout_ms: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            live_enabled: false,
            confirm_timeout_ms: 8_000,
            prefer_single_tx: true,
            bundle_status_poll_ms: 1_000,
            bundle_timeout_ms: 30_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RoundTripConfig {
    pub enabled: bool,
    pub base: String,
    pub quote: String,
    pub amount_lamports: u64,
    pub weight: u32,
    /// Jupiter `maxAccounts` per leg (small enough that all legs fit one tx).
    pub max_accounts: Option<u8>,
}

impl Default for RoundTripConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base: "SOL".into(),
            quote: "USDC".into(),
            amount_lamports: 1_000_000_000,
            weight: 1,
            max_accounts: Some(30),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CrossDexConfig {
    pub enabled: bool,
    pub base: String,
    pub quote: String,
    pub amount_lamports: u64,
    /// Jupiter DEX labels (case-sensitive). Every ordered pair (A, B), A≠B, is scanned.
    pub dexes: Vec<String>,
    pub weight: u32,
    pub max_accounts: Option<u8>,
}

impl Default for CrossDexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base: "SOL".into(),
            quote: "USDC".into(),
            amount_lamports: 1_000_000_000,
            dexes: vec!["Raydium CLMM".into(), "Whirlpool".into(), "Meteora DLMM".into(), "HumidiFi".into()],
            weight: 3,
            max_accounts: Some(30),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TriangularConfig {
    pub enabled: bool,
    /// Token symbols; the cycle closes back to the first (must be SOL).
    pub cycle: Vec<String>,
    pub amount_lamports: u64,
    pub weight: u32,
    pub max_accounts: Option<u8>,
}

impl Default for TriangularConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cycle: vec!["SOL".into(), "USDC".into(), "JUP".into()],
            amount_lamports: 1_000_000_000,
            weight: 1,
            max_accounts: Some(20),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StrategiesConfig {
    pub round_trip: Vec<RoundTripConfig>,
    pub cross_dex: Vec<CrossDexConfig>,
    pub triangular: Vec<TriangularConfig>,
}

impl Default for StrategiesConfig {
    fn default() -> Self {
        Self {
            round_trip: vec![RoundTripConfig::default()],
            cross_dex: vec![CrossDexConfig::default()],
            triangular: vec![TriangularConfig::default()],
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlyphMode {
    #[default]
    Auto,
    Unicode,
    Ascii,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorMode {
    #[default]
    Auto,
    Truecolor,
    Ansi256,
    None,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiConfig {
    pub glyphs: GlyphMode,
    pub color: ColorMode,
    pub fps: u16,
    pub max_graphs: usize,
    /// Mouse clicks/wheel in the TUI (text selection then needs the
    /// terminal's modifier key while dragging).
    pub mouse: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { glyphs: GlyphMode::Auto, color: ColorMode::Auto, fps: 15, max_graphs: 6, mouse: true }
    }
}

/// Real-time market data that does not spend the Jupiter rate limit: pool and
/// Pyth price accounts over the RPC WebSocket (`accountSubscribe`), network
/// stats over RPC, and the Jito tip stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FeedsConfig {
    pub enabled: bool,
    /// Pool accounts: mid price decoded from on-chain state (not executable).
    pub pools: Vec<PoolFeed>,
    /// Pyth push-oracle price accounts (sponsored feeds, shard 0).
    pub oracles: Vec<OracleFeed>,
    /// Minimum interval between recorded samples per feed.
    pub emit_interval_ms: u64,
    /// `getRecentPrioritizationFees` cadence (TPS is polled every 3rd tick).
    pub network_poll_ms: u64,
    /// Jito tip stream (WebSocket); empty = REST polling only.
    pub tip_stream_url: String,
    /// Oracle delivery: `onchain` (Pyth price accounts over the chain
    /// WebSocket) or `hermes` (Pyth Hermes SSE; needs `pyth_api_key_env`,
    /// falls back to `onchain` without it).
    pub oracle_source: OracleSourceKind,
    pub hermes_url: String,
    pub pyth_api_key_env: String,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleSourceKind {
    #[default]
    Onchain,
    Hermes,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolKind {
    Whirlpool,
    RaydiumClmm,
    MeteoraDlmm,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolFeed {
    /// Display name (Jupiter's DEX label).
    pub dex: String,
    pub kind: PoolKind,
    pub address: String,
    /// Token symbols; the pool's mints must be these two (either order).
    pub base: String,
    pub quote: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleFeed {
    /// e.g. `SOL/USD`
    pub symbol: String,
    pub address: String,
    /// Pyth feed id (64 hex chars); checked against the account data.
    pub feed_id: String,
}

impl Default for FeedsConfig {
    fn default() -> Self {
        // Pools: the SOL/USDC pools Jupiter routed through most in the recorded
        // sessions. Oracles: Pyth sponsored-feed PDAs (shard 0). All verified
        // against mainnet account data (owner, mints / feed id, price).
        let pool = |dex: &str, kind, address: &str| PoolFeed {
            dex: dex.into(),
            kind,
            address: address.into(),
            base: "SOL".into(),
            quote: "USDC".into(),
        };
        let oracle = |symbol: &str, address: &str, feed_id: &str| OracleFeed {
            symbol: symbol.into(),
            address: address.into(),
            feed_id: feed_id.into(),
        };
        Self {
            enabled: true,
            pools: vec![
                pool("Whirlpool", PoolKind::Whirlpool, "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE"),
                pool("Raydium CLMM", PoolKind::RaydiumClmm, "CYbD9RaToYMtWKA7QZyoLahnHdWq553Vm62Lh6qWtuxq"),
                pool("Meteora DLMM", PoolKind::MeteoraDlmm, "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR"),
            ],
            oracles: vec![
                oracle(
                    "SOL/USD",
                    "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE",
                    "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
                ),
                oracle(
                    "JUP/USD",
                    "7dbob1psH1iZBS7qPsm3Kwbf5DzSXK8Jyg31CTgTnxH5",
                    "0a0408d619e9380abad35060f9192039ed5042fa6f82301d0e48bb52be830996",
                ),
                oracle(
                    "USDC/USD",
                    "Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX",
                    "eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a",
                ),
            ],
            emit_interval_ms: 500,
            network_poll_ms: 10_000,
            tip_stream_url: "wss://bundles.jito.wtf/api/v1/bundles/tip_stream".into(),
            oracle_source: OracleSourceKind::Onchain,
            hermes_url: "https://hermes.pyth.network".into(),
            pyth_api_key_env: "PYTH_API_KEY".into(),
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerKind {
    /// Market events decide which route to quote (default).
    #[default]
    Event,
    /// Weighted round-robin over strategies (the original scanner; kept for
    /// A/B benchmarks).
    RoundRobin,
}

/// How Jupiter requests are scheduled. See `docs/LATENCY.md`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SchedulerConfig {
    pub kind: SchedulerKind,
    /// Gateway sliding window: requests per window (learned from
    /// `x-ratelimit-*` at runtime; this is the starting value).
    pub window_capacity: u32,
    pub window_ms: u64,
    /// Added to `window_ms` in our own accounting: the gateway counts request
    /// *arrivals*, we count *sends*, and HTTP latency varies by ~1 s.
    pub window_margin_ms: u64,
    /// Window slots never used (other clients of the organisation, skew).
    pub window_safety: u32,
    /// Slots calibration may not use (kept for bursts after market events).
    pub reserve: u32,
    pub max_in_flight: usize,
    /// A move of a route's input below this (bp) is not news.
    pub change_bp: f64,
    pub leg_ttl_ms: u64,
    pub unobservable_ttl_ms: u64,
    /// Every route is re-quoted at least this often (model calibration).
    pub floor_s: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            kind: SchedulerKind::Event,
            window_capacity: 10,
            window_ms: 10_000,
            window_margin_ms: 1_000,
            window_safety: 2,
            reserve: 3,
            max_in_flight: 3,
            change_bp: 0.5,
            leg_ttl_ms: 2_500,
            unobservable_ttl_ms: 1_500,
            floor_s: 90,
        }
    }
}

/// Outbound connections.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkConfig {
    /// `auto`: `HTTPS_PROXY`/`ALL_PROXY` from the environment, else the OS
    /// proxy settings (macOS); `none`: always connect directly;
    /// `http://host:port`: this HTTP proxy for every connection.
    pub proxy: String,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { proxy: "auto".into() }
    }
}

/// Recording database retention (`<data_dir>/mobius.sqlite`). Pruned at
/// startup and every `prune_interval_min` while recording; `--prune` runs it now.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    /// Sessions without activity for longer than this are deleted.
    pub retention_days: u32,
    /// Sessions with trades or executions are kept this long.
    pub keep_trading_days: u32,
    /// Above this size the oldest sessions go first (never the running one or
    /// sessions with trades).
    pub max_db_mb: u64,
    pub prune_interval_min: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self { retention_days: 7, keep_trading_days: 90, max_db_mb: 1024, prune_interval_min: 30 }
    }
}

/// `--research`: three measurements that decide what to build next. It runs
/// on its own (it holds the Jupiter budget while it runs), writes to
/// `<data_dir>/research.sqlite`, and never signs or sends anything.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResearchConfig {
    /// Size ladder: every round quotes one configured route (the enabled
    /// strategies' cycles, in turn) at each of these input sizes.
    pub ladder_sizes_lamports: Vec<u64>,
    pub ladder_every_s: u64,
    /// Cross-chain: the same asset on Solana (Jupiter) and on EVM chains
    /// (Uniswap v3 QuoterV2), bought and sold for this much USDC.
    pub xchain_notional_usd: Vec<u32>,
    pub xchain_every_s: u64,
    pub xchain_assets: Vec<XchainAsset>,
    /// DEX lag: every pool mid from `[feeds]` against the CEX mid. A gap wider
    /// than this opens an episode, and one Jupiter quote on that DEX checks
    /// what is actually executable (pool fees and impact included).
    pub lag_trigger_bps: f64,
    /// A confirming quote without a trigger every this many seconds (control
    /// sample: does the trigger find better prices than chance?). 0 = off.
    pub lag_control_every_s: u64,
    pub lag_confirm_lamports: u64,
    /// At most one confirming quote per pool per this many seconds.
    pub lag_confirm_cooldown_s: u64,
    /// CEX prices after an episode starts (seconds): who moved, DEX or CEX.
    pub lag_markouts_s: Vec<u32>,
    pub lag_okx_ws_url: String,
    pub lag_okx_inst: String,
    /// Binance's public mirror (the main endpoint refuses some locations).
    pub lag_binance_ws_url: String,
    /// Keep the machine awake while researching (macOS `caffeinate`).
    pub keep_awake: bool,
}

impl ResearchConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.ladder_sizes_lamports.is_empty() || self.ladder_sizes_lamports.contains(&0) {
            return Err("research.ladder_sizes_lamports: at least one size, all > 0".into());
        }
        if self.ladder_every_s < 5 || self.xchain_every_s < 5 {
            return Err("research.*_every_s must be ≥ 5".into());
        }
        if self.xchain_notional_usd.contains(&0) {
            return Err("research.xchain_notional_usd must be > 0".into());
        }
        for a in &self.xchain_assets {
            a.solana_mint
                .parse::<Address>()
                .map_err(|e| format!("research.xchain_assets {}: solana_mint: {e}", a.symbol))?;
        }
        if self.lag_trigger_bps.is_nan() || self.lag_trigger_bps <= 0.0 {
            return Err("research.lag_trigger_bps must be > 0".into());
        }
        if self.lag_confirm_lamports == 0 {
            return Err("research.lag_confirm_lamports must be > 0".into());
        }
        Ok(())
    }
}

/// `--canary`: one approved trade through the real LIVE path to prove it
/// works end to end. Profit guards are replaced by a loss bound that is also
/// written into the transaction's minimum output.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CanaryConfig {
    /// Most the trade may lose, SOL-equivalent (fees, tip and price included).
    pub max_loss_lamports: u64,
    /// Input of the SOL → USDC → SOL round trip; 0 = the first enabled
    /// round-trip strategy's amount (else 0.1 SOL).
    pub amount_lamports: u64,
}

impl Default for CanaryConfig {
    fn default() -> Self {
        Self { max_loss_lamports: 500_000, amount_lamports: 0 }
    }
}

/// One asset for the cross-chain comparison.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XchainAsset {
    pub symbol: String,
    pub solana_mint: String,
    pub solana_decimals: u8,
    /// `venue name → Uniswap v3 pool` (the venue gives the chain RPC and QuoterV2).
    pub evm_pools: BTreeMap<String, String>,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            ladder_sizes_lamports: vec![
                10_000_000,
                50_000_000,
                100_000_000,
                250_000_000,
                500_000_000,
                1_000_000_000,
                2_000_000_000,
            ],
            ladder_every_s: 60,
            xchain_notional_usd: vec![25, 250],
            xchain_every_s: 30,
            // Addresses read back on chain 2026-09-22 (symbol, decimals, factory getPool).
            xchain_assets: vec![
                XchainAsset {
                    symbol: "ETH".into(),
                    // Wormhole-bridged ETH on Solana (8 decimals)
                    solana_mint: "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs".into(),
                    solana_decimals: 8,
                    evm_pools: BTreeMap::from([
                        ("base".into(), "0xd0b53D9277642d899DF5C87A3966A349A798F224".into()),
                        ("arbitrum".into(), "0xC6962004f452bE9203591991D15f6b388e09E8D0".into()),
                    ]),
                },
                XchainAsset {
                    symbol: "cbBTC".into(),
                    solana_mint: "cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij".into(),
                    solana_decimals: 8,
                    // Arbitrum's cbBTC/USDC pools hold almost no liquidity
                    evm_pools: BTreeMap::from([("base".into(), "0xfbb6eed8e7aa03b138556eedaf5d271a5e1e43ef".into())]),
                },
            ],
            lag_trigger_bps: 4.0,
            lag_control_every_s: 120,
            lag_confirm_lamports: 100_000_000,
            lag_confirm_cooldown_s: 10,
            lag_markouts_s: vec![1, 5, 30],
            lag_okx_ws_url: "wss://ws.okx.com:8443/ws/v5/public".into(),
            lag_okx_inst: "SOL-USDC".into(),
            lag_binance_ws_url: "wss://data-stream.binance.vision/ws/solusdc@bookTicker".into(),
            keep_awake: true,
        }
    }
}

/// 32-byte Pyth feed id from 64 hex chars (optional `0x`).
pub fn parse_feed_id(s: &str) -> Option<[u8; 32]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config parse error: {0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Config, ConfigError> {
        let t: toml::Table = s.parse().map_err(|e: toml::de::Error| ConfigError::Parse(e.to_string()))?;
        let (t, _) = lift_solana(t).map_err(ConfigError::Invalid)?;
        let c: Config =
            toml::Value::Table(t).try_into().map_err(|e: toml::de::Error| ConfigError::Parse(e.to_string()))?;
        c.validate()?;
        Ok(c)
    }

    /// Where the database and reports go (`general.data_dir`, else [`user_data_dir`]).
    pub fn data_dir(&self) -> PathBuf {
        match self.general.data_dir.trim() {
            "" => user_data_dir(),
            d => PathBuf::from(d),
        }
    }

    pub fn tokens(&self) -> TokenRegistry {
        let mut r = TokenRegistry::defaults();
        for t in &self.tokens {
            r.insert(t.clone());
        }
        r
    }

    /// The address used as taker for `/build` and `simulateTransaction`.
    pub fn simulation_taker(&self) -> Option<Address> {
        self.paper.simulation_taker.as_deref().or(self.wallet.pubkey.as_deref()).and_then(|s| s.parse().ok())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |m: String| Err(ConfigError::Invalid(m));
        let tokens = self.tokens();

        self.jupiter.slippage_spec().map_err(ConfigError::Invalid)?;
        self.profit.guards().map_err(ConfigError::Invalid)?;
        self.research.validate().map_err(ConfigError::Invalid)?;
        parse_decimal(&self.risk.max_daily_loss_usd, 6)
            .map_err(|e| ConfigError::Invalid(format!("risk.max_daily_loss_usd: {e}")))?;
        if self.jupiter.general_rps <= 0.0 || self.rpc.rps <= 0.0 || self.jito.rps <= 0.0 {
            return bad("rate limits must be > 0".into());
        }
        if !(1..=300).contains(&self.jupiter.blockhash_slots_to_expiry) {
            return bad("jupiter.blockhash_slots_to_expiry must be 1..=300".into());
        }
        if let Some(m) = self.jupiter.max_accounts
            && !(1..=64).contains(&m)
        {
            return bad("jupiter.max_accounts must be 1..=64".into());
        }
        let tp = &self.jito.tip_policy;
        if tp.min_lamports < crate::units::JITO_MIN_TIP_LAMPORTS {
            return bad(format!("jito.tip_policy.min_lamports must be ≥ {}", crate::units::JITO_MIN_TIP_LAMPORTS));
        }
        if tp.max_lamports < tp.min_lamports {
            return bad("jito.tip_policy.max_lamports < min_lamports".into());
        }
        if !["p25", "p50", "p75", "p95", "p99", "ema50"].contains(&tp.percentile.as_str()) {
            return bad(format!("jito.tip_policy.percentile `{}` unknown", tp.percentile));
        }
        for key in [&self.wallet.pubkey, &self.paper.simulation_taker].into_iter().flatten() {
            key.parse::<Address>().map_err(|e| ConfigError::Invalid(format!("bad public key `{key}`: {e}")))?;
        }
        let sc = &self.scheduler;
        if sc.window_capacity <= sc.window_safety + sc.reserve || sc.window_ms < 1_000 || sc.max_in_flight == 0 {
            return bad(
                "scheduler: window_capacity must exceed window_safety + reserve; window_ms ≥ 1000; max_in_flight ≥ 1"
                    .into(),
            );
        }
        if sc.max_in_flight as u32 >= sc.window_capacity {
            return bad("scheduler.max_in_flight must be below window_capacity".into());
        }
        for (name, v) in &self.venues {
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
            {
                return bad(format!("venues.{name}: names use a-z, 0-9, _ and -"));
            }
            if !v.rest_url.starts_with("https://") && !v.rest_url.starts_with("http://") {
                return bad(format!("venues.{name}.rest_url must be an http(s) URL"));
            }
            let evm_addr =
                |a: &str| a.len() == 42 && a.starts_with("0x") && a[2..].chars().all(|c| c.is_ascii_hexdigit());
            match v.kind {
                VenueKind::Okx | VenueKind::Binance => {
                    if v.enabled && v.markets.is_empty() {
                        return bad(format!("venues.{name}.markets: list at least one instrument"));
                    }
                }
                VenueKind::Evm => {
                    if v.chain_id.is_none() {
                        return bad(format!("venues.{name}.chain_id is required for kind = \"evm\""));
                    }
                    if !evm_addr(&v.quoter) {
                        return bad(format!("venues.{name}.quoter must be a 0x… address"));
                    }
                    if v.enabled && v.pools.is_empty() {
                        return bad(format!("venues.{name}.pools: list at least one pool address"));
                    }
                    if !v.router.is_empty() && !evm_addr(&v.router) {
                        return bad(format!("venues.{name}.router must be a 0x… address"));
                    }
                    if let Some(p) = v.pools.iter().find(|p| !evm_addr(p)) {
                        return bad(format!("venues.{name}.pools: `{p}` is not a 0x… address"));
                    }
                }
            }
            if v.taker_fee_bps > 1_000 {
                return bad(format!("venues.{name}.taker_fee_bps must be ≤ 1000"));
            }
            if v.trading {
                return bad(format!(
                    "venues.{name}: trading on {:?} is not implemented yet (market data only); set trading = false",
                    v.kind
                ));
            }
        }
        let st = &self.storage;
        if st.retention_days == 0
            || st.keep_trading_days < st.retention_days
            || st.max_db_mb < 16
            || st.prune_interval_min == 0
        {
            return bad(
                "storage: retention_days ≥ 1, keep_trading_days ≥ retention_days, max_db_mb ≥ 16, prune_interval_min ≥ 1"
                    .into(),
            );
        }
        let proxy = self.network.proxy.trim();
        if !["auto", "none", ""].contains(&proxy) && !proxy.starts_with("http://") {
            return bad(format!("network.proxy must be \"auto\", \"none\" or http://host:port, got `{proxy}`"));
        }
        if sc.kind == SchedulerKind::Event && !self.feeds.enabled {
            return bad("scheduler.kind = \"event\" needs [feeds] enabled (pool events drive it)".into());
        }
        let f = &self.feeds;
        if f.emit_interval_ms == 0 || f.network_poll_ms < 1_000 {
            return bad("feeds.emit_interval_ms must be > 0 and feeds.network_poll_ms ≥ 1000".into());
        }
        for p in &f.pools {
            p.address.parse::<Address>().map_err(|e| ConfigError::Invalid(format!("feeds.pools `{}`: {e}", p.dex)))?;
            for sym in [&p.base, &p.quote] {
                if tokens.get(sym).is_none() {
                    return bad(format!("feeds.pools `{}`: unknown token {sym}", p.dex));
                }
            }
        }
        for o in &f.oracles {
            o.address
                .parse::<Address>()
                .map_err(|e| ConfigError::Invalid(format!("feeds.oracles `{}`: {e}", o.symbol)))?;
            if parse_feed_id(&o.feed_id).is_none() {
                return bad(format!("feeds.oracles `{}`: feed_id must be 64 hex chars", o.symbol));
            }
        }

        let need_sol_base = |base: &str, what: &str| -> Result<(), ConfigError> {
            if base != "SOL" {
                return bad(format!("{what}: base must be SOL (fees are paid in SOL; PnL is in lamports)"));
            }
            Ok(())
        };
        let need_token = |sym: &str, what: &str| -> Result<(), ConfigError> {
            if tokens.get(sym).is_none() {
                return bad(format!("{what}: unknown token `{sym}` (add it under [[tokens]])"));
            }
            Ok(())
        };
        for (i, s) in self.strategies.round_trip.iter().enumerate() {
            let w = format!("strategies.round_trip[{i}]");
            need_sol_base(&s.base, &w)?;
            need_token(&s.quote, &w)?;
            if s.amount_lamports == 0 {
                return bad(format!("{w}: amount_lamports must be > 0"));
            }
        }
        for (i, s) in self.strategies.cross_dex.iter().enumerate() {
            let w = format!("strategies.cross_dex[{i}]");
            need_sol_base(&s.base, &w)?;
            need_token(&s.quote, &w)?;
            if s.enabled && s.dexes.len() < 2 {
                return bad(format!("{w}: needs at least two dexes"));
            }
            if s.amount_lamports == 0 {
                return bad(format!("{w}: amount_lamports must be > 0"));
            }
        }
        for (i, s) in self.strategies.triangular.iter().enumerate() {
            let w = format!("strategies.triangular[{i}]");
            if s.cycle.len() < 3 {
                return bad(format!("{w}: cycle needs ≥ 3 tokens"));
            }
            need_sol_base(&s.cycle[0], &w)?;
            for t in &s.cycle {
                need_token(t, &w)?;
            }
            if s.amount_lamports == 0 {
                return bad(format!("{w}: amount_lamports must be > 0"));
            }
        }

        // Sending gates. PAPER never needs a private key.
        if self.general.mode.sends_transactions() {
            if !self.execution.live_enabled {
                return bad(format!(
                    "mode {} sends transactions and requires execution.live_enabled = true",
                    self.general.mode.label()
                ));
            }
            if self.wallet.keypair_path.as_deref().unwrap_or("").is_empty() {
                return bad(format!("mode {} requires wallet.keypair_path", self.general.mode.label()));
            }
            if self.paper.simulation_taker.is_some() {
                return bad("paper.simulation_taker must be unset outside PAPER (simulate as the real signer)".into());
            }
        }
        Ok(())
    }

    /// Redacted, loggable summary.
    pub fn summary(&self) -> String {
        let sim_taker = self.simulation_taker().map(|a| a.short()).unwrap_or_else(|| "none".into());
        format!(
            "mode={} live_enabled={} jupiter={} rps={} slippage={} rpc_rps={} jito_tip={:?}/{} taker={} strategies=rt:{} xdex:{} tri:{}",
            self.general.mode.label(),
            self.execution.live_enabled,
            self.jupiter.base_url,
            self.jupiter.general_rps,
            self.jupiter.slippage,
            self.rpc.rps,
            self.jito.tip_policy.kind,
            self.jito.tip_policy.percentile,
            sim_taker,
            self.strategies.round_trip.iter().filter(|s| s.enabled).count(),
            self.strategies.cross_dex.iter().filter(|s| s.enabled).count(),
            self.strategies.triangular.iter().filter(|s| s.enabled).count(),
        )
    }
}

// ─── Layered files ──────────────────────────────────────────────────────────

/// The shared, checked-in defaults (relative to the working directory).
pub const REPO_CONFIG_PATH: &str = "config/mobius.toml";

/// Per-user directory: `$MOBIUS_HOME`, else `$XDG_CONFIG_HOME/mobius`, else
/// `~/.config/mobius`.
pub fn user_dir() -> PathBuf {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("MOBIUS_HOME")
        .or_else(|| var("XDG_CONFIG_HOME").map(|d| d.join("mobius")))
        .or_else(|| var("HOME").or_else(|| var("USERPROFILE")).map(|h| h.join(".config").join("mobius")))
        .unwrap_or_else(|| PathBuf::from(".mobius"))
}

/// Per-user data (database, latency reports): `$MOBIUS_HOME/data`, else
/// `$XDG_DATA_HOME/mobius`, else `~/.local/share/mobius`. Independent of the
/// directory the program is started from, so an installed binary works anywhere.
pub fn user_data_dir() -> PathBuf {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("MOBIUS_HOME")
        .map(|h| h.join("data"))
        .or_else(|| var("XDG_DATA_HOME").map(|d| d.join("mobius")))
        .or_else(|| var("HOME").or_else(|| var("USERPROFILE")).map(|h| h.join(".local").join("share").join("mobius")))
        .unwrap_or_else(|| PathBuf::from("data"))
}

/// The user's own settings: a small TOML file with only what differs from the
/// repository defaults. `MOBIUS_CONFIG` points elsewhere.
pub fn user_config_path() -> PathBuf {
    std::env::var_os("MOBIUS_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| user_dir().join("config.toml"))
}

/// The user's secrets (`KEY=VALUE`, mode 0600).
pub fn user_env_path() -> PathBuf {
    user_dir().join(".env")
}

/// Written when the first-use wizard completes.
pub fn setup_marker_path() -> PathBuf {
    user_dir().join("setup-complete")
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Default,
    Repo,
    User,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Layer::Default => "default",
            Layer::Repo => "repo",
            Layer::User => "user",
        }
    }
}

/// The effective configuration and where each value came from.
#[derive(Clone, Debug)]
pub struct Layered {
    pub config: Config,
    /// Defaults + repository file: what the user file is a delta against.
    pub below_user: Config,
    /// Files read, in order (missing files are skipped).
    pub files: Vec<(Layer, PathBuf)>,
    /// Files that still put Solana sections at the top level (old layout;
    /// read until v0.4, `--migrate-config` moves them under `[venues.solana]`).
    pub legacy_solana: Vec<PathBuf>,
    /// Dotted key → the layer that set it; keys absent here are built-in defaults.
    pub origins: BTreeMap<String, Layer>,
}

impl Layered {
    pub fn origin(&self, key: &str) -> Layer {
        self.origins.get(key).copied().unwrap_or(Layer::Default)
    }

    /// Every leaf of the effective config as `(dotted key, value, layer)`.
    pub fn entries(&self) -> Vec<(String, toml::Value, Layer)> {
        let mut out = Vec::new();
        if let Ok(toml::Value::Table(t)) = toml::Value::try_from(&self.config) {
            leaves("", &t, &mut |k, v| out.push((k.to_string(), v.clone(), self.origin(k))));
        }
        out
    }
}

fn read_table(path: &Path) -> Result<Option<toml::Table>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ConfigError::Parse(format!("{}: {e}", path.display()))),
    };
    text.parse::<toml::Table>().map(Some).map_err(|e| ConfigError::Parse(format!("{}: {e}", path.display())))
}

/// The Solana stack's sections. Since v0.2 they live under `[venues.solana]`
/// (e.g. `[venues.solana.rpc]`); the top-level form is still read until v0.4.
pub const SOLANA_SECTIONS: [&str; 5] = ["rpc", "jupiter", "jito", "feeds", "wallet"];

/// Lift `[venues.solana.<section>]` to the internal top-level `<section>`.
/// Returns the table and whether it used the old top-level form.
pub fn lift_solana(mut t: toml::Table) -> Result<(toml::Table, bool), String> {
    let legacy = SOLANA_SECTIONS.iter().any(|s| t.contains_key(*s));
    let solana = match t.get_mut("venues") {
        Some(toml::Value::Table(v)) => v.remove("solana"),
        _ => None,
    };
    if matches!(t.get("venues"), Some(toml::Value::Table(v)) if v.is_empty()) {
        t.remove("venues");
    }
    let Some(solana) = solana else { return Ok((t, legacy)) };
    let toml::Value::Table(mut solana) = solana else { return Err("[venues.solana] must be a table".into()) };
    if let Some(k) = solana.remove("kind")
        && k.as_str() != Some("solana")
    {
        return Err(format!("[venues.solana] kind must be \"solana\", got {k}"));
    }
    for (k, v) in solana {
        if !SOLANA_SECTIONS.contains(&k.as_str()) {
            return Err(format!("[venues.solana] has no `{k}` (sections: {})", SOLANA_SECTIONS.join(", ")));
        }
        if t.contains_key(&k) {
            return Err(format!("[{k}] is set both at the top level and as [venues.solana.{k}]; keep one"));
        }
        t.insert(k, v);
    }
    Ok((t, legacy))
}

/// Tables merge key by key; anything else (including arrays) replaces.
fn merge(dst: &mut toml::Table, src: toml::Table) {
    for (k, v) in src {
        match (dst.get_mut(&k), v) {
            (Some(toml::Value::Table(d)), toml::Value::Table(s)) => merge(d, s),
            (_, v) => {
                dst.insert(k, v);
            }
        }
    }
}

fn leaves(prefix: &str, t: &toml::Table, f: &mut dyn FnMut(&str, &toml::Value)) {
    for (k, v) in t {
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match v {
            toml::Value::Table(sub) => leaves(&key, sub, f),
            v => f(&key, v),
        }
    }
}

fn to_config(t: toml::Table) -> Result<Config, ConfigError> {
    toml::Value::Table(t).try_into().map_err(|e: toml::de::Error| ConfigError::Parse(e.to_string()))
}

/// Load `repo` then `user` over the built-in defaults and validate the result.
/// Either file may be missing. Errors name the file they come from.
pub fn load_layered(repo: &Path, user: &Path) -> Result<Layered, ConfigError> {
    // start from the built-in defaults so maps (venues) merge key by key too
    let mut merged = match toml::Value::try_from(Config::default()) {
        Ok(toml::Value::Table(t)) => t,
        _ => toml::Table::new(),
    };
    let mut files = Vec::new();
    let mut origins = BTreeMap::new();
    let mut below_user = None;
    let mut legacy_solana = Vec::new();
    for (layer, path) in [(Layer::Repo, repo), (Layer::User, user)] {
        if layer == Layer::User {
            below_user = Some(to_config(merged.clone()).map_err(|e| in_file(e, repo))?);
        }
        let Some(t) = read_table(path)? else { continue };
        let (t, legacy) = lift_solana(t).map_err(|e| ConfigError::Invalid(format!("{}: {e}", path.display())))?;
        if legacy {
            legacy_solana.push(path.to_path_buf());
        }
        // each file must be valid on its own terms (unknown keys, types),
        // read over the defaults so a partial venue table is complete
        let mut alone = match toml::Value::try_from(Config::default()) {
            Ok(toml::Value::Table(d)) => d,
            _ => toml::Table::new(),
        };
        merge(&mut alone, t.clone());
        to_config(alone).map_err(|e| in_file(e, path))?;
        leaves("", &t, &mut |k, _| {
            origins.insert(k.to_string(), layer);
        });
        merge(&mut merged, t);
        files.push((layer, path.to_path_buf()));
    }
    let config = to_config(merged)?;
    config.validate()?;
    Ok(Layered { config, below_user: below_user.unwrap_or_default(), files, legacy_solana, origins })
}

fn in_file(e: ConfigError, path: &Path) -> ConfigError {
    match e {
        ConfigError::Parse(m) => ConfigError::Parse(format!("{}: {m}", path.display())),
        other => other,
    }
}

/// TOML for a user file holding only what `config` changes relative to
/// `below` (defaults + repository file). Arrays are written whole when they
/// differ. A value cannot be *unset* below the user layer.
pub fn user_delta(below: &Config, config: &Config) -> Result<String, ConfigError> {
    fn diff(below: &toml::Table, cur: &toml::Table) -> toml::Table {
        let mut out = toml::Table::new();
        for (k, v) in cur {
            match (below.get(k), v) {
                (Some(toml::Value::Table(b)), toml::Value::Table(c)) => {
                    let d = diff(b, c);
                    if !d.is_empty() {
                        out.insert(k.clone(), toml::Value::Table(d));
                    }
                }
                (Some(b), v) if b == v => {}
                (_, v) => {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        out
    }
    let table = |c: &Config| match toml::Value::try_from(c) {
        Ok(toml::Value::Table(t)) => Ok(t),
        Ok(_) => Err(ConfigError::Invalid("config did not serialize to a table".into())),
        Err(e) => Err(ConfigError::Invalid(e.to_string())),
    };
    let d = diff(&table(below)?, &table(config)?);
    toml::to_string_pretty(&d).map_err(|e| ConfigError::Invalid(e.to_string()))
}

/// `scheme://host[:port]` plus `/…` when there is more: safe to print for any
/// URL (provider keys hide in paths and query strings).
pub fn display_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return "(not a URL)".into() };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].rsplit('@').next().unwrap_or("");
    let more = rest[end..].trim_start_matches('/');
    format!("{scheme}://{host}{}", if more.is_empty() { "" } else { "/…" })
}

#[cfg(test)]
mod solana_layout_tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn old_and_new_solana_layouts_load_the_same_and_can_mix_across_layers() {
        let dir = std::env::temp_dir().join(format!("mobius-layout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let none = dir.join("absent.toml");
        let old = write(&dir, "old.toml", "[rpc]\nrps = 7.0\n[jupiter]\nslippage = \"10\"\n");
        let new =
            write(&dir, "new.toml", "[venues.solana.rpc]\nrps = 7.0\n[venues.solana.jupiter]\nslippage = \"10\"\n");
        let a = load_layered(&none, &old).unwrap();
        let b = load_layered(&none, &new).unwrap();
        assert_eq!(toml::Value::try_from(&a.config).unwrap(), toml::Value::try_from(&b.config).unwrap());
        assert_eq!(a.legacy_solana, vec![old.clone()]);
        assert!(b.legacy_solana.is_empty());
        // repo file in the new layout, the user's old file still overrides it
        let repo = write(&dir, "repo.toml", "[venues.solana.rpc]\nrps = 3.0\n");
        assert_eq!(load_layered(&repo, &old).unwrap().config.rpc.rps, 7.0);
        // one section in both places is refused
        let both = write(&dir, "both.toml", "[rpc]\nrps = 1.0\n[venues.solana.rpc]\nrps = 2.0\n");
        assert!(load_layered(&none, &both).unwrap_err().to_string().contains("keep one"));
        assert!(Config::from_toml("[venues.solana.rpc]\nrps = 4.0\n").unwrap().rpc.rps == 4.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_paper_and_valid() {
        let c = Config::from_toml("").unwrap();
        assert_eq!(c.general.mode, Mode::Paper);
        assert!(!c.execution.live_enabled);
        assert_eq!(c.jupiter.slippage_spec().unwrap(), SlippageSpec::Rtse);
    }

    #[test]
    fn live_requires_explicit_gate_and_keypair() {
        let e = Config::from_toml("[general]\nmode = \"live\"\n").unwrap_err();
        assert!(e.to_string().contains("live_enabled"), "{e}");
        let e = Config::from_toml("[general]\nmode = \"confirm\"\n[execution]\nlive_enabled = true\n").unwrap_err();
        assert!(e.to_string().contains("keypair_path"), "{e}");
        let ok = Config::from_toml(
            "[general]\nmode = \"live\"\n[execution]\nlive_enabled = true\n[wallet]\nkeypair_path = \"/k.json\"\n",
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn rejects_unknown_fields_and_bad_values() {
        assert!(Config::from_toml("[general]\nmdoe = \"live\"\n").is_err());
        assert!(Config::from_toml("[jupiter]\nslippage = \"lots\"\n").is_err());
        assert!(Config::from_toml("[profit]\nmin_profit_usd = \"0.0000001\"\n").is_err());
        assert!(Config::from_toml("[jito.tip_policy]\nmin_lamports = 10\n").is_err());
        assert!(Config::from_toml("[[strategies.triangular]]\ncycle = [\"USDC\",\"SOL\",\"JUP\"]\n").is_err());
        assert!(Config::from_toml("[[strategies.triangular]]\ncycle = [\"SOL\",\"USDC\",\"WIF\"]\n").is_err());
        assert!(Config::from_toml("[wallet]\npubkey = \"nope\"\n").is_err());
    }

    #[test]
    fn summary_has_no_secrets() {
        let c = Config::from_toml("[wallet]\nkeypair_path = \"/secret/path.json\"\n").unwrap();
        assert!(!c.summary().contains("/secret/path.json"));
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mobius-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn layers_merge_tables_by_key_and_replace_arrays() {
        let d = tmpdir("merge");
        let repo = write(
            &d,
            "repo.toml",
            "[rpc]\nurl = \"https://repo.example\"\nrps = 2.0\n[[strategies.cross_dex]]\ndexes = [\"A\", \"B\", \"C\"]\n",
        );
        let user = write(&d, "user.toml", "[rpc]\nrps = 5.0\n[[strategies.cross_dex]]\ndexes = [\"X\", \"Y\"]\n");
        let l = load_layered(&repo, &user).unwrap();
        assert_eq!(l.config.rpc.url, "https://repo.example", "kept from the repo layer");
        assert_eq!(l.config.rpc.rps, 5.0, "user wins");
        assert_eq!(l.config.rpc.ws_url, RpcConfig::default().ws_url, "built-in default");
        assert_eq!(l.config.strategies.cross_dex.len(), 1);
        assert_eq!(l.config.strategies.cross_dex[0].dexes, ["X", "Y"], "arrays replace, never append");
        assert_eq!(
            (l.origin("rpc.url"), l.origin("rpc.rps"), l.origin("rpc.ws_url")),
            (Layer::Repo, Layer::User, Layer::Default)
        );
        assert_eq!(l.below_user.rpc.rps, 2.0);
        assert_eq!(l.files.len(), 2);
        // a missing user file is fine
        let l = load_layered(&repo, &d.join("absent.toml")).unwrap();
        assert_eq!(l.config.rpc.rps, 2.0);
    }

    #[test]
    fn errors_name_the_file_and_the_merged_result_is_validated() {
        let d = tmpdir("errors");
        let repo = write(&d, "repo.toml", "[rpc]\nrps = 2.0\n");
        let user = write(&d, "user.toml", "[rpc]\nrsp = 5.0\n");
        let e = load_layered(&repo, &user).unwrap_err().to_string();
        assert!(e.contains("user.toml") && e.contains("rsp"), "{e}");
        // LIVE needs the explicit gate even when the pieces come from different files
        let user = write(&d, "user.toml", "[general]\nmode = \"live\"\n[wallet]\nkeypair_path = \"/k.json\"\n");
        assert!(load_layered(&repo, &user).unwrap_err().to_string().contains("live_enabled"));
    }

    #[test]
    fn a_user_delta_reproduces_the_config_and_holds_only_changes() {
        let d = tmpdir("delta");
        let repo = write(&d, "repo.toml", "[rpc]\nurl = \"https://repo.example\"\n");
        let below = load_layered(&repo, &d.join("none.toml")).unwrap().config;
        let mut cfg = below.clone();
        cfg.rpc.rps = 7.0;
        cfg.wallet.pubkey = Some("So11111111111111111111111111111111111111112".into());
        cfg.strategies.round_trip[0].amount_lamports = 5;
        let delta = user_delta(&below, &cfg).unwrap();
        assert!(!delta.contains("repo.example") && !delta.contains("[jupiter]"), "{delta}");
        let user = write(&d, "user.toml", &delta);
        let back = load_layered(&repo, &user).unwrap().config;
        assert_eq!(toml::Value::try_from(&back).unwrap(), toml::Value::try_from(&cfg).unwrap());
        assert_eq!(user_delta(&below, &below).unwrap().trim(), "");
    }

    #[test]
    fn display_url_never_shows_paths_queries_or_credentials() {
        assert_eq!(display_url("https://mainnet.helius-rpc.com/?api-key=SECRET"), "https://mainnet.helius-rpc.com/…");
        assert_eq!(display_url("https://x.quiknode.pro/SECRET/"), "https://x.quiknode.pro/…");
        assert_eq!(display_url("wss://user:SECRET@host:8900"), "wss://host:8900");
        assert_eq!(display_url("https://api.jup.ag"), "https://api.jup.ag");
    }

    #[test]
    fn venues_merge_per_key_over_the_built_in_okx_and_new_ones_can_be_added() {
        let d = tmpdir("venues");
        let repo = write(&d, "repo.toml", "");
        let user = write(
            &d,
            "user.toml",
            "[venues.okx]\nmarkets = [\"BTC-USDT\"]\n[venues.okx_eu]\nkind = \"okx\"\nrest_url = \"https://my.okx.com\"\nmarkets = [\"ETH-EUR\"]\n",
        );
        let l = load_layered(&repo, &user).unwrap();
        let okx = &l.config.venues["okx"];
        assert_eq!(okx.markets, ["BTC-USDT"]);
        assert_eq!(okx.rest_url, "https://www.okx.com", "untouched keys keep the built-in value");
        assert_eq!(okx.api_key_env, "OKX_API_KEY");
        assert_eq!(l.config.venues["okx_eu"].rest_url, "https://my.okx.com");
        assert_eq!(l.origin("venues.okx.markets"), Layer::User);
        assert_eq!(l.origin("venues.okx.rest_url"), Layer::Default);
        // unknown connector kinds and trading without a connector are refused
        let user = write(&d, "user.toml", "[venues.kr]\nkind = \"kraken\"\nrest_url = \"https://api.kraken.com\"\n");
        assert!(load_layered(&repo, &user).unwrap_err().to_string().contains("kraken"));
        let user = write(&d, "user.toml", "[venues.okx]\ntrading = true\n");
        assert!(load_layered(&repo, &user).unwrap_err().to_string().contains("not implemented"));
    }

    /// An installed binary started outside the checkout never sees
    /// config/mobius.toml, so the built-in defaults must be the same settings.
    #[test]
    fn the_shared_config_file_equals_the_built_in_defaults() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/mobius.toml");
        let file = Config::from_toml(&std::fs::read_to_string(path).unwrap()).unwrap();
        let delta = user_delta(&Config::default(), &file).unwrap();
        assert!(delta.trim().is_empty(), "config/mobius.toml differs from Config::default():\n{delta}");
    }

    #[test]
    fn data_dir_defaults_to_the_per_user_directory() {
        let c = Config::default();
        assert_eq!(c.data_dir(), user_data_dir());
        assert!(c.data_dir().is_absolute() || std::env::var_os("HOME").is_none());
        let c = Config::from_toml("[general]\ndata_dir = \"data\"\n").unwrap();
        assert_eq!(c.data_dir(), PathBuf::from("data"));
    }

    #[test]
    fn proxy_setting_is_validated() {
        assert!(Config::from_toml("[network]\nproxy = \"none\"\n").is_ok());
        assert!(Config::from_toml("[network]\nproxy = \"http://127.0.0.1:7890\"\n").is_ok());
        assert!(Config::from_toml("[network]\nproxy = \"socks5://h:1\"\n").is_err());
    }
}
