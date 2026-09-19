//! Exact wire shapes of Jupiter Swap API V2 responses. Nothing outside this
//! crate sees these types.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildResponse {
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    #[serde(default)]
    pub swap_mode: Option<String>,
    pub slippage_bps: u16,
    #[serde(default)]
    pub price_impact_pct: Option<String>,
    pub route_plan: Vec<RoutePlanStep>,
    #[serde(default)]
    pub compute_budget_instructions: Vec<WireInstruction>,
    #[serde(default)]
    pub setup_instructions: Vec<WireInstruction>,
    pub swap_instruction: WireInstruction,
    #[serde(default)]
    pub cleanup_instruction: Option<WireInstruction>,
    #[serde(default)]
    pub other_instructions: Vec<WireInstruction>,
    /// Jupiter's own landing tip (only with `tipAmount`, which we never send).
    #[serde(default)]
    pub tip_instruction: Option<WireInstruction>,
    #[serde(default)]
    pub addresses_by_lookup_table_address: Option<BTreeMap<String, Vec<String>>>,
    pub blockhash_with_metadata: BlockhashWithMetadata,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutePlanStep {
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub bps: Option<u16>,
    #[serde(rename = "swapInfo")]
    pub swap_info: SwapInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInfo {
    pub amm_key: String,
    pub label: String,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireInstruction {
    pub program_id: String,
    pub accounts: Vec<WireAccountMeta>,
    /// base64
    pub data: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireAccountMeta {
    pub pubkey: String,
    pub is_signer: bool,
    pub is_writable: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockhashWithMetadata {
    pub blockhash: Vec<u8>,
    pub last_valid_block_height: u64,
    #[serde(default)]
    pub fetched_at: Option<FetchedAt>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FetchedAt {
    pub secs_since_epoch: i64,
    pub nanos_since_epoch: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorBody {
    #[serde(default)]
    pub error: Option<serde_json::Value>,
    #[serde(default)]
    pub message: Option<String>,
}

impl ErrorBody {
    pub fn text(&self) -> String {
        match (&self.error, &self.message) {
            (Some(serde_json::Value::String(s)), _) => s.clone(),
            (Some(v), _) => v.to_string(),
            (None, Some(m)) => m.clone(),
            (None, None) => String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceEntry {
    pub usd_price: f64,
    #[serde(default)]
    pub block_id: Option<u64>,
    #[serde(default)]
    pub decimals: Option<u8>,
}
