//! EVM chains over JSON-RPC: Uniswap v3 pool state (`slot0`, `liquidity`)
//! and exact quotes from QuoterV2 (`quoteExactInputSingle`, which simulates
//! the swap: pool fee and price impact included). Read-only; no signing.
//!
//! Function selectors are constants (first 4 bytes of the Keccak-256 of the
//! signature), each checked against mainnet contracts on 2026-09-19.

use searcher_telemetry::{LimiterConfig, RateLimiter};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// `slot0()`
pub const SLOT0: &str = "0x3850c7bd";
/// `liquidity()`
pub const LIQUIDITY: &str = "0x1a686502";
/// `token0()`, `token1()`, `fee()`
pub const TOKEN0: &str = "0x0dfe1681";
pub const TOKEN1: &str = "0xd21220a7";
pub const FEE: &str = "0xddca3f43";
/// ERC-20 `symbol()`, `decimals()`
pub const SYMBOL: &str = "0x95d89b41";
pub const DECIMALS: &str = "0x313ce567";
/// QuoterV2 `quoteExactInputSingle((address,address,uint256,uint24,uint160))`
pub const QUOTE_EXACT_INPUT_SINGLE: &str = "0xc6a5026a";

#[derive(Debug, thiserror::Error)]
pub enum EvmError {
    #[error("evm transport: {0}")]
    Transport(String),
    #[error("evm rpc {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("evm decode: {0}")]
    Decode(String),
}

/// A 20-byte address as a 32-byte ABI word (hex, no 0x).
pub fn word_address(addr: &str) -> String {
    format!("{:0>64}", addr.trim_start_matches("0x").to_ascii_lowercase())
}

pub fn word_u128(v: u128) -> String {
    format!("{v:064x}")
}

fn hex_bytes(s: &str) -> Result<Vec<u8>, EvmError> {
    let s = s.trim_start_matches("0x");
    if !s.len().is_multiple_of(2) {
        return Err(EvmError::Decode("odd hex length".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| EvmError::Decode(e.to_string())))
        .collect()
}

/// Word `i` of ABI-encoded return data.
pub fn word(data: &[u8], i: usize) -> Result<&[u8], EvmError> {
    data.get(i * 32..i * 32 + 32).ok_or_else(|| EvmError::Decode(format!("return data too short for word {i}")))
}

/// An unsigned word that must fit in u128 (token amounts, liquidity).
pub fn word_u128_of(w: &[u8]) -> Result<u128, EvmError> {
    if w[..16].iter().any(|b| *b != 0) {
        return Err(EvmError::Decode("value exceeds u128".into()));
    }
    Ok(u128::from_be_bytes(w[16..32].try_into().unwrap()))
}

/// Any unsigned word as f64 (prices; ~15 significant digits).
pub fn word_f64(w: &[u8]) -> f64 {
    w.iter().fold(0.0, |acc, b| acc * 256.0 + *b as f64)
}

/// A signed word whose value fits in i32 (e.g. an int24 tick).
pub fn word_i32(w: &[u8]) -> i32 {
    i32::from_be_bytes(w[28..32].try_into().unwrap())
}

pub fn word_to_address(w: &[u8]) -> String {
    let hex: String = w[12..32].iter().map(|b| format!("{b:02x}")).collect();
    format!("0x{hex}")
}

/// ABI `string` return value (offset, length, bytes).
pub fn decode_string(data: &[u8]) -> Result<String, EvmError> {
    let off = word_u128_of(word(data, 0)?)? as usize;
    let len =
        data.get(off..off + 32).map(word_u128_of).ok_or_else(|| EvmError::Decode("string length".into()))?? as usize;
    let bytes = data.get(off + 32..off + 32 + len).ok_or_else(|| EvmError::Decode("string bytes".into()))?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

pub struct EvmRpc {
    http: reqwest::Client,
    url: String,
    limiter: RateLimiter,
    id: AtomicU64,
}

impl EvmRpc {
    /// `rps`: stay under the provider's free limit (publicnode tolerates a few per second).
    pub fn new(url: &str, rps: f64) -> Result<Self, EvmError> {
        let mut b = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent(concat!("mobius/", env!("CARGO_PKG_VERSION")));
        if let Some(p) = searcher_telemetry::proxy::fallback_https_proxy() {
            b = b.proxy(reqwest::Proxy::all(p).map_err(|e| EvmError::Transport(e.to_string()))?);
        }
        if searcher_telemetry::proxy::direct() {
            b = b.no_proxy();
        }
        Ok(Self {
            http: b.build().map_err(|e| EvmError::Transport(e.to_string()))?,
            url: url.to_string(),
            limiter: RateLimiter::new("evm.rpc", LimiterConfig::new(rps, 2)),
            id: AtomicU64::new(1),
        })
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, EvmError> {
        self.limiter.acquire().await;
        let id = self.id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp =
            self.http.post(&self.url).json(&body).send().await.map_err(|e| EvmError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let v: Value = resp.json().await.map_err(|e| EvmError::Transport(format!("http {status}: {e}")))?;
        if let Some(e) = v.get("error") {
            return Err(EvmError::Rpc {
                code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: e.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
            });
        }
        v.get("result").cloned().ok_or_else(|| EvmError::Decode(format!("no result (http {status})")))
    }

    fn quantity(v: &Value) -> Result<u64, EvmError> {
        let s = v.as_str().ok_or_else(|| EvmError::Decode("quantity".into()))?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| EvmError::Decode(e.to_string()))
    }

    pub async fn chain_id(&self) -> Result<u64, EvmError> {
        Self::quantity(&self.request("eth_chainId", json!([])).await?)
    }

    /// Latest block: (number, timestamp in unix seconds).
    pub async fn latest_block(&self) -> Result<(u64, u64), EvmError> {
        let b = self.request("eth_getBlockByNumber", json!(["latest", false])).await?;
        Ok((
            Self::quantity(b.get("number").unwrap_or(&Value::Null))?,
            Self::quantity(b.get("timestamp").unwrap_or(&Value::Null))?,
        ))
    }

    /// `eth_call` at `block` (`"latest"` or a hex number), raw return data.
    pub async fn call(&self, to: &str, data: &str, block: &str) -> Result<Vec<u8>, EvmError> {
        let r = self.request("eth_call", json!([{"to": to, "data": data}, block])).await?;
        hex_bytes(r.as_str().ok_or_else(|| EvmError::Decode("eth_call result".into()))?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub address: String,
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniV3Pool {
    pub address: String,
    pub token0: Token,
    pub token1: Token,
    /// Pool fee in hundredths of a bip (500 = 0.05 %).
    pub fee: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PoolState {
    pub sqrt_price_x96: f64,
    pub tick: i32,
    pub liquidity: u128,
    /// Block the state was read at.
    pub block: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    pub amount_out: u128,
    pub ticks_crossed: u32,
    pub gas_estimate: u64,
    /// Block the quote was simulated at.
    pub block: u64,
}

async fn token(rpc: &EvmRpc, address: String) -> Result<Token, EvmError> {
    let symbol = decode_string(&rpc.call(&address, SYMBOL, "latest").await?)?;
    let decimals = word_u128_of(word(&rpc.call(&address, DECIMALS, "latest").await?, 0)?)? as u8;
    Ok(Token { address, symbol, decimals })
}

impl UniV3Pool {
    /// Read the pool's tokens (with symbol and decimals) and fee from chain.
    pub async fn load(rpc: &EvmRpc, address: &str) -> Result<UniV3Pool, EvmError> {
        let t0 = word_to_address(word(&rpc.call(address, TOKEN0, "latest").await?, 0)?);
        let t1 = word_to_address(word(&rpc.call(address, TOKEN1, "latest").await?, 0)?);
        let fee = word_u128_of(word(&rpc.call(address, FEE, "latest").await?, 0)?)? as u32;
        Ok(UniV3Pool {
            address: address.to_ascii_lowercase(),
            token0: token(rpc, t0).await?,
            token1: token(rpc, t1).await?,
            fee,
        })
    }

    /// `slot0` and `liquidity` at one block (both reads pinned to it).
    pub async fn state(&self, rpc: &EvmRpc) -> Result<PoolState, EvmError> {
        let (block, _) = rpc.latest_block().await?;
        let at = format!("0x{block:x}");
        let s0 = rpc.call(&self.address, SLOT0, &at).await?;
        let liq = rpc.call(&self.address, LIQUIDITY, &at).await?;
        Ok(PoolState {
            sqrt_price_x96: word_f64(word(&s0, 0)?),
            tick: word_i32(word(&s0, 1)?),
            liquidity: word_u128_of(word(&liq, 0)?)?,
            block,
        })
    }

    /// Mid price of `base` in the other token, human units (e.g. USDC per
    /// WETH), from `sqrtPriceX96`. Not executable: quotes are.
    pub fn mid(&self, sqrt_price_x96: f64, base: &str) -> Option<f64> {
        let p = (sqrt_price_x96 / 2f64.powi(96)).powi(2); // token1 per token0, raw units
        let p = p * 10f64.powi(self.token0.decimals as i32 - self.token1.decimals as i32);
        if base.eq_ignore_ascii_case(&self.token0.symbol) {
            Some(p)
        } else if base.eq_ignore_ascii_case(&self.token1.symbol) {
            (p > 0.0).then(|| 1.0 / p)
        } else {
            None
        }
    }

    /// `WETH/USDC` style name with the non-stable token as base.
    pub fn market(&self) -> String {
        let stable = |s: &str| matches!(s, "USDC" | "USDT" | "DAI" | "USDC.e");
        if stable(&self.token0.symbol) && !stable(&self.token1.symbol) {
            format!("{}/{}", self.token1.symbol, self.token0.symbol)
        } else {
            format!("{}/{}", self.token0.symbol, self.token1.symbol)
        }
    }

    pub fn token(&self, symbol: &str) -> Option<&Token> {
        [&self.token0, &self.token1].into_iter().find(|t| t.symbol.eq_ignore_ascii_case(symbol))
    }
}

/// Calldata of `quoteExactInputSingle` for this pool, selling `amount_in`
/// raw units of `token_in` (no price limit).
pub fn quote_calldata(pool: &UniV3Pool, token_in: &Token, amount_in: u128) -> String {
    let token_out = if token_in.address == pool.token0.address { &pool.token1 } else { &pool.token0 };
    format!(
        "{QUOTE_EXACT_INPUT_SINGLE}{}{}{}{}{}",
        word_address(&token_in.address),
        word_address(&token_out.address),
        word_u128(amount_in),
        word_u128(pool.fee as u128),
        word_u128(0)
    )
}

pub fn parse_quote(data: &[u8], block: u64) -> Result<Quote, EvmError> {
    Ok(Quote {
        amount_out: word_u128_of(word(data, 0)?)?,
        ticks_crossed: word_u128_of(word(data, 2)?)? as u32,
        gas_estimate: word_u128_of(word(data, 3)?)? as u64,
        block,
    })
}

/// Exact output of selling `amount_in` of `token_in` into the pool, simulated
/// by QuoterV2 at the latest block.
pub async fn quote(
    rpc: &EvmRpc,
    quoter: &str,
    pool: &UniV3Pool,
    token_in: &Token,
    amount_in: u128,
) -> Result<Quote, EvmError> {
    let (block, _) = rpc.latest_block().await?;
    let data = rpc.call(quoter, &quote_calldata(pool, token_in, amount_in), &format!("0x{block:x}")).await?;
    parse_quote(&data, block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_pool() -> UniV3Pool {
        UniV3Pool {
            address: "0xd0b53d9277642d899df5c87a3966a349a798f224".into(),
            token0: Token {
                address: "0x4200000000000000000000000000000000000006".into(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".into(),
                symbol: "USDC".into(),
                decimals: 6,
            },
            fee: 500,
        }
    }

    fn w(hex: &str) -> Vec<u8> {
        hex_bytes(hex).unwrap()
    }

    #[test]
    fn quote_calldata_matches_the_abi_encoding_used_on_chain() {
        // the same bytes returned 2634.74 USDC for 1 WETH on Base (2026-09-19)
        let p = base_pool();
        let data = quote_calldata(&p, &p.token0, 10u128.pow(18));
        assert_eq!(
            data,
            concat!(
                "0xc6a5026a",
                "0000000000000000000000004200000000000000000000000000000000000006",
                "000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                "0000000000000000000000000000000000000000000000000de0b6b3a7640000",
                "00000000000000000000000000000000000000000000000000000000000001f4",
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
        );
    }

    #[test]
    fn a_quote_answer_decodes() {
        // amountOut 2634.740205 USDC, sqrtPriceX96After, 1 tick crossed, gas 111730
        let data = w(concat!(
            "000000000000000000000000000000000000000000000000000000009d0af1ed",
            "000000000000000000000000000000000000000000035d5ece7138434590562c",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "000000000000000000000000000000000000000000000000000000000001b472",
        ));
        let q = parse_quote(&data, 7).unwrap();
        assert_eq!((q.amount_out, q.ticks_crossed, q.gas_estimate, q.block), (2_634_740_205, 1, 111_730, 7));
    }

    #[test]
    fn mid_price_from_sqrt_price_in_both_orientations() {
        // Base WETH/USDC 0.05 %, sqrtPriceX96 read on chain 2026-09-19
        let p = base_pool();
        let mid = p.mid(4_067_842_555_164_011_011_798_453.0, "WETH").unwrap();
        assert!((mid - 2636.1).abs() < 1.0, "{mid}");
        let inv = p.mid(4_067_842_555_164_011_011_798_453.0, "USDC").unwrap();
        assert!((inv * mid - 1.0).abs() < 1e-9);
        assert_eq!(p.mid(1.0, "DAI"), None);
        assert_eq!(p.market(), "WETH/USDC");
        // Ethereum's pool has USDC as token0: the market name still puts WETH first
        let mut e = base_pool();
        std::mem::swap(&mut e.token0, &mut e.token1);
        assert_eq!(e.market(), "WETH/USDC");
        let m = e.mid(1_543_258_361_466_207_989_055_270_047_900_004.0, "WETH").unwrap();
        assert!((m - 2635.6).abs() < 1.5, "{m}");
    }

    #[test]
    fn words_decode_addresses_strings_and_signed_ticks() {
        let a = w("000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda02913");
        assert_eq!(word_to_address(&a), "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
        // "USDC" as an ABI string
        let s = w(concat!(
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000004",
            "5553444300000000000000000000000000000000000000000000000000000000",
        ));
        assert_eq!(decode_string(&s).unwrap(), "USDC");
        let neg = w("fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffcf2c5");
        assert_eq!(word_i32(&neg), -199_995);
        assert!(word_u128_of(&[0xff; 32]).is_err());
    }
}
