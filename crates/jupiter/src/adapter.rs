//! Wire → domain conversion. The only place Jupiter JSON semantics live.

use crate::wire::{BuildResponse, WireInstruction};
use base64::Engine;
use searcher_core::ix::{LegInstructions, RawAccountMeta, RawInstruction, decode_cu_price};
use searcher_core::model::{DexFilter, Hop, Leg, RoutingMode, SlippageSpec};
use searcher_core::{Address, Ppm, Ts};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AdaptError {
    #[error("bad address `{0}`")]
    Address(String),
    #[error("bad amount `{0}`")]
    Amount(String),
    #[error("bad base64 instruction data")]
    Base64,
    #[error("blockhash must be 32 bytes, got {0}")]
    Blockhash(usize),
    #[error("response mints do not match the request")]
    MintMismatch,
    /// Seen on mainnet: a 200 response whose route returns nothing.
    #[error("route returns zero output")]
    ZeroOutput,
}

fn addr(s: &str) -> Result<Address, AdaptError> {
    s.parse().map_err(|_| AdaptError::Address(s.to_string()))
}

fn amount(s: &str) -> Result<u64, AdaptError> {
    s.parse().map_err(|_| AdaptError::Amount(s.to_string()))
}

pub fn instruction(w: &WireInstruction) -> Result<RawInstruction, AdaptError> {
    Ok(RawInstruction {
        program_id: addr(&w.program_id)?,
        accounts: w
            .accounts
            .iter()
            .map(|a| {
                Ok(RawAccountMeta { pubkey: addr(&a.pubkey)?, is_signer: a.is_signer, is_writable: a.is_writable })
            })
            .collect::<Result<_, AdaptError>>()?,
        data: base64::engine::general_purpose::STANDARD.decode(&w.data).map_err(|_| AdaptError::Base64)?,
    })
}

/// `"0.001"` (decimal ratio) → ppm, truncated. Display-only field; tolerant.
pub fn ratio_to_ppm(s: &str) -> Ppm {
    let s = s.trim();
    let (neg, body) = s.strip_prefix('-').map_or((false, s), |b| (true, b));
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let parsed = (|| {
        let i: i64 = if int.is_empty() { 0 } else { int.parse().ok()? };
        let mut f6: String = frac.chars().take(6).collect();
        while f6.len() < 6 {
            f6.push('0');
        }
        let f: i64 = f6.parse().ok()?;
        i.checked_mul(1_000_000)?.checked_add(f)
    })();
    match parsed {
        Some(v) => Ppm(if neg { -v } else { v }),
        None => s.parse::<f64>().map(|f| Ppm((f * 1e6) as i64)).unwrap_or(Ppm::ZERO),
    }
}

pub struct LegContext {
    pub index: u8,
    pub slippage_spec: SlippageSpec,
    pub mode: RoutingMode,
    pub dex_filter: DexFilter,
    pub quoted_at: Ts,
    pub latency_ms: u32,
    pub request_id: Option<String>,
    pub expect_input: Address,
    pub expect_output: Address,
}

/// Convert a `/build` response into a domain `Leg` plus its instructions.
pub fn to_leg(r: &BuildResponse, ctx: LegContext) -> Result<(Leg, LegInstructions), AdaptError> {
    let input_mint = addr(&r.input_mint)?;
    let output_mint = addr(&r.output_mint)?;
    if input_mint != ctx.expect_input || output_mint != ctx.expect_output {
        return Err(AdaptError::MintMismatch);
    }
    if amount(&r.out_amount)? == 0 {
        return Err(AdaptError::ZeroOutput);
    }
    let hops = r
        .route_plan
        .iter()
        .map(|s| {
            let bps = s.bps.unwrap_or_else(|| (s.percent.unwrap_or(0.0) * 100.0).round().clamp(0.0, 10_000.0) as u16);
            Ok(Hop {
                amm_key: addr(&s.swap_info.amm_key)?,
                label: s.swap_info.label.clone(),
                input_mint: addr(&s.swap_info.input_mint)?,
                output_mint: addr(&s.swap_info.output_mint)?,
                in_amount: amount(&s.swap_info.in_amount)?,
                out_amount: amount(&s.swap_info.out_amount)?,
                bps,
            })
        })
        .collect::<Result<Vec<_>, AdaptError>>()?;

    let compute_budget =
        r.compute_budget_instructions.iter().map(instruction).collect::<Result<Vec<_>, AdaptError>>()?;
    let cu_price_micro = compute_budget.iter().find_map(decode_cu_price);

    let blockhash: [u8; 32] = r
        .blockhash_with_metadata
        .blockhash
        .as_slice()
        .try_into()
        .map_err(|_| AdaptError::Blockhash(r.blockhash_with_metadata.blockhash.len()))?;

    let lookup_tables = r
        .addresses_by_lookup_table_address
        .iter()
        .flatten()
        .map(|(k, v)| Ok((addr(k)?, v.iter().map(|a| addr(a)).collect::<Result<Vec<_>, _>>()?)))
        .collect::<Result<Vec<_>, AdaptError>>()?;

    let leg = Leg {
        index: ctx.index,
        input_mint,
        output_mint,
        in_amount: amount(&r.in_amount)?,
        out_amount: amount(&r.out_amount)?,
        min_out: amount(&r.other_amount_threshold)?,
        slippage_bps: r.slippage_bps,
        slippage_spec: ctx.slippage_spec,
        price_impact: r.price_impact_pct.as_deref().map(ratio_to_ppm).unwrap_or(Ppm::ZERO),
        hops,
        mode: ctx.mode,
        dex_filter: ctx.dex_filter,
        quoted_at: ctx.quoted_at,
        latency_ms: ctx.latency_ms,
        cu_price_micro,
        last_valid_block_height: r.blockhash_with_metadata.last_valid_block_height,
        request_id: ctx.request_id,
    };
    let ixs = LegInstructions {
        compute_budget,
        setup: r.setup_instructions.iter().map(instruction).collect::<Result<_, _>>()?,
        swap: instruction(&r.swap_instruction)?,
        cleanup: r.cleanup_instruction.as_ref().map(instruction).transpose()?,
        other: r.other_instructions.iter().map(instruction).collect::<Result<_, _>>()?,
        lookup_tables,
        blockhash,
        last_valid_block_height: r.blockhash_with_metadata.last_valid_block_height,
    };
    Ok((leg, ixs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::address::well_known;

    const FIXTURE: &str = include_str!("../../../fixtures/jupiter_build_sol_usdc_rtse.json");

    fn ctx() -> LegContext {
        LegContext {
            index: 0,
            slippage_spec: SlippageSpec::Rtse,
            mode: RoutingMode::Normal,
            dex_filter: DexFilter::Any,
            quoted_at: Ts(1),
            latency_ms: 21,
            request_id: Some("req".into()),
            expect_input: well_known::addr(well_known::WSOL_MINT),
            expect_output: well_known::addr(well_known::USDC_MINT),
        }
    }

    #[test]
    fn real_build_fixture_adapts() {
        let r: BuildResponse = serde_json::from_str(FIXTURE).unwrap();
        let (leg, ixs) = to_leg(&r, ctx()).unwrap();
        assert_eq!(leg.in_amount, 100_000_000);
        assert_eq!(leg.out_amount, 10_538_567);
        assert_eq!(leg.min_out, 10_522_759);
        assert_eq!(leg.slippage_bps, 15);
        assert!(leg.min_out < leg.out_amount);
        assert_eq!(leg.cu_price_micro, Some(2_719));
        assert!(!leg.hops.is_empty());
        assert!(leg.hops.iter().all(|h| h.bps <= 10_000));
        assert_eq!(leg.hops[0].label, "Byreal");
        assert_eq!(ixs.swap.program_id.to_string(), well_known::JUPITER_V6_PROGRAM);
        assert_eq!(ixs.setup.len(), 4);
        assert!(ixs.cleanup.is_some());
        assert_eq!(ixs.lookup_tables.len(), 4);
        assert_eq!(ixs.last_valid_block_height, 426_039_712);
        assert_eq!(ixs.blockhash[0], 59);
        // tolerance = (out - min)/out
        assert_eq!(leg.tolerance(), Ppm::ratio(15_808, 10_538_567).unwrap());
    }

    #[test]
    fn mint_mismatch_is_rejected() {
        let r: BuildResponse = serde_json::from_str(FIXTURE).unwrap();
        let mut c = ctx();
        std::mem::swap(&mut c.expect_input, &mut c.expect_output);
        assert_eq!(to_leg(&r, c).unwrap_err(), AdaptError::MintMismatch);
    }

    #[test]
    fn corrupted_fields_are_errors_not_panics() {
        let mut v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        v["outAmount"] = "12x".into();
        let r: BuildResponse = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(to_leg(&r, ctx()), Err(AdaptError::Amount(_))));
        v["outAmount"] = "1".into();
        v["blockhashWithMetadata"]["blockhash"] = serde_json::json!([1, 2, 3]);
        let r: BuildResponse = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(to_leg(&r, ctx()), Err(AdaptError::Blockhash(3))));
        v.as_object_mut().unwrap().remove("swapInstruction");
        assert!(serde_json::from_value::<BuildResponse>(v).is_err());
    }

    #[test]
    fn ratio_parsing() {
        assert_eq!(ratio_to_ppm("0.001"), Ppm(1_000));
        assert_eq!(ratio_to_ppm("0"), Ppm(0));
        assert_eq!(ratio_to_ppm("-0.0000123456"), Ppm(-12));
        assert_eq!(ratio_to_ppm("1.5"), Ppm(1_500_000));
        assert_eq!(ratio_to_ppm("1e-3"), Ppm(1_000));
        assert_eq!(ratio_to_ppm("garbage"), Ppm(0));
    }

    #[test]
    fn a_route_returning_nothing_is_an_error_not_a_price() {
        let mut v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        v["outAmount"] = serde_json::json!("0");
        let r: BuildResponse = serde_json::from_value(v).unwrap();
        assert!(matches!(to_leg(&r, ctx()), Err(AdaptError::ZeroOutput)));
    }
}
