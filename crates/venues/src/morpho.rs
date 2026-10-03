//! Morpho Blue, read-only: its liquidation events, what one paid, and the
//! contract's own answer to "could this position be liquidated at block N?".
//!
//! Liquidation there is first come, first served: once a position's debt
//! exceeds `collateral × oracle price × LLTV`, anyone may repay debt and
//! seize collateral worth `repaid × LIF` at the oracle price. Nothing here
//! signs or sends.

use crate::evm::{Call, EvmError, EvmRpc, Log, word, word_address, word_f64, word_to_address, word_u128, word_u128_of};
use crate::evm_sign::{hex, keccak256, selector};

pub const LIQUIDATE_EVENT: &str = "Liquidate(bytes32,address,address,uint256,uint256,uint256,uint256,uint256)";
const ID_TO_MARKET_PARAMS: &str = "idToMarketParams(bytes32)";
const LIQUIDATE: &str = "liquidate((address,address,address,address,uint256),address,uint256,uint256,bytes)";
/// Revert reason of `liquidate` on a position that is not liquidatable (`ErrorsLib.HEALTHY_POSITION`).
const HEALTHY: &str = "position is healthy";
/// Sender of the probing call: holds nothing and has approved nothing.
const PROBE_FROM: &str = "0x00000000000000000000000000000000004d4f42";

/// First topic of `Liquidate` logs.
pub fn liquidate_topic() -> String {
    format!("0x{}", hex(&keccak256(LIQUIDATE_EVENT.as_bytes())))
}

/// One `Liquidate` event. Amounts are raw token units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Liquidation {
    pub block: u64,
    pub log_index: u32,
    pub tx: String,
    /// Market id (`0x…`, 32 bytes).
    pub market: String,
    /// `msg.sender` of `liquidate`: usually the liquidator's contract.
    pub caller: String,
    pub borrower: String,
    pub repaid_assets: u128,
    pub seized_assets: u128,
    pub bad_debt_assets: u128,
}

impl Liquidation {
    pub fn decode(log: &Log) -> Result<Liquidation, EvmError> {
        let [_, market, caller, borrower] = log.topics.as_slice() else {
            return Err(EvmError::Decode(format!("Liquidate log with {} topics", log.topics.len())));
        };
        let address = |t: &str| format!("0x{}", &t[t.len().saturating_sub(40)..]);
        let amount = |i| word(&log.data, i).and_then(word_u128_of);
        Ok(Liquidation {
            block: log.block,
            log_index: log.index,
            tx: log.tx.clone(),
            market: market.clone(),
            caller: address(caller),
            borrower: address(borrower),
            repaid_assets: amount(0)?,
            seized_assets: amount(2)?,
            bad_debt_assets: amount(3)?,
        })
    }
}

/// A market's immutable parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct MarketParams {
    pub loan_token: String,
    pub collateral_token: String,
    pub oracle: String,
    /// Liquidation loan-to-value as a fraction (0.86).
    pub lltv: f64,
    /// The five ABI words as returned, reused verbatim as a call argument.
    raw: String,
}

impl MarketParams {
    pub async fn load(rpc: &EvmRpc, morpho: &str, market: &str) -> Result<MarketParams, EvmError> {
        let data = format!("{}{}", selector(ID_TO_MARKET_PARAMS), market.trim_start_matches("0x"));
        Self::decode(&rpc.call(morpho, &data, "latest").await?)
    }

    pub fn decode(data: &[u8]) -> Result<MarketParams, EvmError> {
        let p = MarketParams {
            loan_token: word_to_address(word(data, 0)?),
            collateral_token: word_to_address(word(data, 1)?),
            oracle: word_to_address(word(data, 2)?),
            lltv: word_f64(word(data, 4)?) / 1e18,
            raw: hex(data.get(..160).unwrap_or_default()),
        };
        if p.lltv <= 0.0 || p.lltv >= 1.0 {
            return Err(EvmError::Decode(format!("market parameters: LLTV {}", p.lltv)));
        }
        Ok(p)
    }

    /// Liquidation incentive factor: seized collateral is worth
    /// `repaid × LIF` at the oracle price (`min(1.15, 1 / (1 − 0.3 × (1 − LLTV)))`).
    pub fn lif(&self) -> f64 {
        (1.0 / (1.0 - 0.3 * (1.0 - self.lltv))).min(1.15)
    }

    /// Calldata of a `liquidate` that repays one borrow share of `borrower`.
    /// It can never succeed from [`PROBE_FROM`]; where it fails tells whether
    /// the position was liquidatable.
    fn probe(&self, borrower: &str) -> String {
        format!(
            "{}{}{}{}{}{}{}",
            selector(LIQUIDATE),
            self.raw,
            word_address(borrower),
            word_u128(0),   // seizedAssets
            word_u128(1),   // repaidShares
            word_u128(288), // offset of the empty callback data
            word_u128(0)
        )
    }

    /// Could `borrower` be liquidated in the state at the end of block
    /// `number`? Asked of the contract itself (interest accrued, oracle
    /// read), so there is no health formula of ours to get wrong.
    pub async fn liquidatable(
        &self,
        rpc: &EvmRpc,
        morpho: &str,
        borrower: &str,
        number: u64,
    ) -> Result<bool, EvmError> {
        match rpc.try_call(PROBE_FROM, morpho, &self.probe(borrower), number).await? {
            Call::Reverted(why) if why == HEALTHY => Ok(false),
            // past the health check: it stopped at the repayment transfer
            Call::Reverted(why) if why.starts_with("transfer") => Ok(true),
            Call::Reverted(why) => Err(EvmError::Decode(format!("liquidate probe reverted: {why}"))),
            Call::Returned(_) => Err(EvmError::Decode("liquidate probe did not revert".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        crate::evm_sign::unhex(s).unwrap()
    }

    // Base, transaction 0xfdf5fec3…18c5eb, block 52,121,149 (2026-10-03): mBASIS/USDC, LLTV 86 %.
    const PARAMS: &str = "000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda029130000000000000000000000001c2757c1fef1038428b5bef062495ce94bbe92b2000000000000000000000000c8b785c7524e79cf876f18a9035fc778a40d3bc900000000000000000000000046415998764c29ab2a25cbea6254146d50d226870000000000000000000000000000000000000000000000000bef55718ad60000";

    fn log() -> Log {
        Log {
            block: 52_121_149,
            index: 0x1a3,
            tx: "0xfdf5fec344a0c4e721a876522f758c8e95b0f070586fd375cea8ca783718c5eb".into(),
            topics: vec![
                "0xa4946ede45d0c6f06a0f5ce92c9ad3b4751452d2fe0e25010783bcab57a67e41".into(),
                "0x45f3b5688e7ba25071f78d1ce51d1b893faa3c86897b12204cdff3af6b3611f8".into(),
                "0x000000000000000000000000b3cf873d27b171c7331c3b4a13a0789c8886314a".into(),
                "0x000000000000000000000000b89350430cc8898b95b8c69f13dd93a42347c7c8".into(),
            ],
            data: unhex(concat!(
                "00000000000000000000000000000000000000000000000000000000087671c0",
                "00000000000000000000000000000000000000000000000000006c4b772e094f",
                "000000000000000000000000000000000000000000000006977a02716d80119b",
                "0000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000000",
            )),
        }
    }

    #[test]
    fn the_event_topic_is_the_one_seen_on_chain() {
        assert_eq!(liquidate_topic(), log().topics[0]);
    }

    #[test]
    fn a_mainnet_liquidation_decodes() {
        let l = Liquidation::decode(&log()).unwrap();
        assert_eq!(l.market, "0x45f3b5688e7ba25071f78d1ce51d1b893faa3c86897b12204cdff3af6b3611f8");
        assert_eq!(l.caller, "0xb3cf873d27b171c7331c3b4a13a0789c8886314a");
        assert_eq!(l.borrower, "0xb89350430cc8898b95b8c69f13dd93a42347c7c8");
        assert_eq!(l.repaid_assets, 141_980_096); // 141.98 USDC
        assert_eq!(l.seized_assets, 121_595_503_775_334_797_723); // 121.60 mBASIS
        assert_eq!(l.bad_debt_assets, 0);
        let mut short = log();
        short.topics.pop();
        assert!(Liquidation::decode(&short).is_err());
    }

    #[test]
    fn market_parameters_and_the_incentive() {
        let p = MarketParams::decode(&unhex(PARAMS)).unwrap();
        assert_eq!(p.loan_token, "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"); // USDC
        assert_eq!(p.oracle, "0xc8b785c7524e79cf876f18a9035fc778a40d3bc9");
        assert!((p.lltv - 0.86).abs() < 1e-12);
        assert!((p.lif() - 1.043_841).abs() < 1e-6, "{}", p.lif());
        // low LLTVs are capped at 15 %
        let low = MarketParams { lltv: 0.385, ..p.clone() };
        assert_eq!(low.lif(), 1.15);
        assert!(MarketParams::decode(&unhex(&PARAMS[..128])).is_err());
    }

    #[test]
    fn the_probe_is_the_call_that_answered_on_base() {
        // this calldata reverted "transferFrom reverted" at block 52,121,148
        // and "position is healthy" at 52,121,147
        let p = MarketParams::decode(&unhex(PARAMS)).unwrap();
        let data = p.probe("0xB89350430cC8898b95b8c69F13dd93A42347c7C8");
        assert_eq!(
            data,
            format!(
                "0xd8eabcb8{PARAMS}{}{}{}{}{}",
                "000000000000000000000000b89350430cc8898b95b8c69f13dd93a42347c7c8",
                "0000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000001",
                "0000000000000000000000000000000000000000000000000000000000000120",
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
        );
        assert_eq!(selector(ID_TO_MARKET_PARAMS), "0x2c3c9157");
    }
}
