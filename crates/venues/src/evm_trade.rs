//! Swapping on an EVM chain through Uniswap's SwapRouter02: approve, then
//! `exactInputSingle`. Every swap is simulated with `eth_call` from our own
//! address first (the router's own output must reach `min_out`), gas is
//! estimated, and only then is the transaction signed and sent. Sending needs
//! a real-account [`TradePermit`]: EVM chains have no demo environment.

use crate::TradePermit;
use crate::evm::{EvmError, EvmRpc, Token, UniV3Pool, word, word_u128, word_u128_of};
use crate::evm_sign::{EvmSigner, SignError, Tx1559, hex, parse_address, selector, unhex};
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum TradeError {
    #[error(transparent)]
    Evm(#[from] EvmError),
    #[error(transparent)]
    Sign(#[from] SignError),
    #[error("refused: {0}")]
    Refused(String),
    #[error("simulation: {0}")]
    Simulation(String),
}

fn word_addr(a: &str) -> String {
    format!("{:0>64}", a.trim_start_matches("0x").to_ascii_lowercase())
}

/// SwapRouter02 `exactInputSingle((tokenIn, tokenOut, fee, recipient,
/// amountIn, amountOutMinimum, sqrtPriceLimitX96))`, no price limit.
pub fn swap_calldata(pool: &UniV3Pool, token_in: &Token, recipient: &str, amount_in: u128, min_out: u128) -> String {
    let token_out = if token_in.address == pool.token0.address { &pool.token1 } else { &pool.token0 };
    format!(
        "{}{}{}{}{}{}{}{}",
        selector("exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))"),
        word_addr(&token_in.address),
        word_addr(&token_out.address),
        word_u128(pool.fee as u128),
        word_addr(recipient),
        word_u128(amount_in),
        word_u128(min_out),
        word_u128(0)
    )
}

/// ERC-20 `approve(spender, amount)`.
pub fn approve_calldata(spender: &str, amount: u128) -> String {
    format!("{}{}{}", selector("approve(address,uint256)"), word_addr(spender), word_u128(amount))
}

fn qty(v: u128) -> String {
    format!("0x{v:x}")
}

fn quantity(v: &Value) -> Result<u128, EvmError> {
    let s = v.as_str().ok_or_else(|| EvmError::Decode("quantity".into()))?;
    u128::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| EvmError::Decode(e.to_string()))
}

/// ERC-20 `allowance(owner, spender)`.
pub async fn allowance(rpc: &EvmRpc, token: &str, owner: &str, spender: &str) -> Result<u128, EvmError> {
    let data = format!("{}{}{}", selector("allowance(address,address)"), word_addr(owner), word_addr(spender));
    word_u128_of(word(&rpc.call(token, &data, "latest").await?, 0)?)
}

/// ERC-20 `balanceOf(owner)`.
pub async fn balance_of(rpc: &EvmRpc, token: &str, owner: &str) -> Result<u128, EvmError> {
    let data = format!("{}{}", selector("balanceOf(address)"), word_addr(owner));
    word_u128_of(word(&rpc.call(token, &data, "latest").await?, 0)?)
}

/// `eth_call` as `from` (with `value` wei): the return data, or the revert.
pub async fn simulate(rpc: &EvmRpc, from: &str, to: &str, data: &str, value: u128) -> Result<Vec<u8>, EvmError> {
    let r =
        rpc.request("eth_call", json!([{"from": from, "to": to, "data": data, "value": qty(value)}, "latest"])).await?;
    unhex(r.as_str().unwrap_or_default()).map_err(|e| EvmError::Decode(e.to_string()))
}

/// (max priority fee, max fee) per gas: the node's tip suggestion and twice
/// the latest base fee plus the tip (room for a few full blocks).
pub async fn fees(rpc: &EvmRpc) -> Result<(u128, u128), EvmError> {
    let block = rpc.request("eth_getBlockByNumber", json!(["latest", false])).await?;
    let base = quantity(block.get("baseFeePerGas").unwrap_or(&Value::Null))?;
    let tip = quantity(&rpc.request("eth_maxPriorityFeePerGas", json!([])).await?)?;
    Ok((tip, base * 2 + tip))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub success: bool,
    pub block: u64,
    pub gas_used: u128,
    pub effective_gas_price: u128,
}

pub async fn receipt(rpc: &EvmRpc, tx_hash: &str) -> Result<Option<Receipt>, EvmError> {
    let r = rpc.request("eth_getTransactionReceipt", json!([tx_hash])).await?;
    if r.is_null() {
        return Ok(None);
    }
    Ok(Some(Receipt {
        success: r.get("status").and_then(Value::as_str) == Some("0x1"),
        block: quantity(r.get("blockNumber").unwrap_or(&Value::Null))? as u64,
        gas_used: quantity(r.get("gasUsed").unwrap_or(&Value::Null))?,
        effective_gas_price: quantity(r.get("effectiveGasPrice").unwrap_or(&Value::Null)).unwrap_or(0),
    }))
}

/// One account on one chain, able to approve and swap.
pub struct EvmTrader {
    pub rpc: EvmRpc,
    pub signer: EvmSigner,
    pub chain_id: u64,
    pub router: String,
}

impl EvmTrader {
    fn check_permit(permit: &TradePermit) -> Result<(), TradeError> {
        if permit.is_demo() {
            return Err(TradeError::Refused(
                "EVM chains have no demo environment: use PAPER (quotes) or a real-account permit".into(),
            ));
        }
        Ok(())
    }

    /// Sign and send a call to `to`, after simulating it and estimating gas.
    async fn send(&self, to: &str, data: &str) -> Result<String, TradeError> {
        let from = self.signer.address().to_string();
        let gas = self
            .rpc
            .request("eth_estimateGas", json!([{"from": from, "to": to, "data": data}]))
            .await
            .map_err(|e| TradeError::Simulation(e.to_string()))?;
        let gas_limit = (quantity(&gas)? as f64 * 1.2) as u64;
        let nonce = quantity(&self.rpc.request("eth_getTransactionCount", json!([from, "pending"])).await?)? as u64;
        let (tip, max_fee) = fees(&self.rpc).await?;
        let tx = Tx1559 {
            chain_id: self.chain_id,
            nonce,
            max_priority_fee_per_gas: tip,
            max_fee_per_gas: max_fee,
            gas_limit,
            to: parse_address(to)?,
            value: 0,
            data: unhex(data)?,
        };
        let (raw, hash) = self.signer.sign_1559(&tx)?;
        let sent = self.rpc.request("eth_sendRawTransaction", json!([format!("0x{}", hex(&raw))])).await?;
        let expect = format!("0x{}", hex(&hash));
        match sent.as_str() {
            Some(h) if h.eq_ignore_ascii_case(&expect) => Ok(expect),
            other => Err(TradeError::Refused(format!("node returned {other:?}, expected {expect}"))),
        }
    }

    /// Let the router spend `amount` of `token` (only when the allowance is short).
    pub async fn ensure_allowance(
        &self,
        permit: &TradePermit,
        token: &Token,
        amount: u128,
    ) -> Result<Option<String>, TradeError> {
        Self::check_permit(permit)?;
        if allowance(&self.rpc, &token.address, self.signer.address(), &self.router).await? >= amount {
            return Ok(None);
        }
        self.send(&token.address, &approve_calldata(&self.router, amount)).await.map(Some)
    }

    /// Sell `amount_in` of `token_in` into `pool`; refuses unless the
    /// simulated output reaches `min_out`. Returns the transaction hash.
    pub async fn swap(
        &self,
        permit: &TradePermit,
        pool: &UniV3Pool,
        token_in: &Token,
        amount_in: u128,
        min_out: u128,
    ) -> Result<String, TradeError> {
        Self::check_permit(permit)?;
        let data = swap_calldata(pool, token_in, self.signer.address(), amount_in, min_out);
        let out = simulate(&self.rpc, self.signer.address(), &self.router, &data, 0)
            .await
            .map_err(|e| TradeError::Simulation(e.to_string()))?;
        let simulated = word_u128_of(word(&out, 0)?)?;
        if simulated < min_out {
            return Err(TradeError::Simulation(format!("output {simulated} < min_out {min_out}")));
        }
        self.send(&self.router, &data).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> UniV3Pool {
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

    #[test]
    fn swap_and_approve_calldata_follow_the_router_abi() {
        let p = pool();
        let d = swap_calldata(&p, &p.token0, "0x00000000000000000000000000000000000000aa", 10u128.pow(17), 263_000_000);
        // 4-byte selector + 7 words
        assert_eq!(d.len(), 2 + 8 + 7 * 64);
        assert_eq!(&d[..10], "0x04e45aaf", "SwapRouter02 exactInputSingle");
        let words: Vec<&str> = (0..7).map(|i| &d[10 + i * 64..10 + (i + 1) * 64]).collect();
        assert!(words[0].ends_with("4200000000000000000000000000000000000006"));
        assert!(words[1].ends_with("833589fcd6edb6e08f4c7c32d4f71b54bda02913"));
        assert_eq!(u128::from_str_radix(words[2], 16).unwrap(), 500);
        assert!(words[3].ends_with("aa"));
        assert_eq!(u128::from_str_radix(words[4], 16).unwrap(), 10u128.pow(17));
        assert_eq!(u128::from_str_radix(words[5], 16).unwrap(), 263_000_000);
        assert_eq!(u128::from_str_radix(words[6], 16).unwrap(), 0);
        let a = approve_calldata("0x2626664c2603336E57B271c5C0b26F421741e481", u128::MAX);
        assert_eq!(&a[..10], "0x095ea7b3", "ERC-20 approve");
        assert!(a.ends_with(&"f".repeat(32)));
    }

    #[test]
    fn evm_trading_refuses_a_demo_permit() {
        let mut v = searcher_core::config::VenueConfig::okx();
        v.trading = true;
        let demo = TradePermit::check(searcher_core::model::Mode::Paper, false, &v).unwrap();
        assert!(matches!(EvmTrader::check_permit(&demo), Err(TradeError::Refused(_))));
    }
}
