//! Simulation result interpretation and failure classification.

use crate::assemble::AssembledTx;
use base64::Engine;
use searcher_core::model::{SimFailure, SimFailureClass, TxSim};
use searcher_core::units::cu_limit_with_margin;
use searcher_core::{Address, Ppm};
use searcher_market::SimulateOutcome;
use serde_json::Value;

/// Jupiter aggregator `SlippageToleranceExceeded` custom error (0x1771).
const JUP_SLIPPAGE: i64 = 6001;
/// Jupiter aggregator `InsufficientFunds` (0x1788): the route's input amount
/// exceeds the source account balance (e.g. an earlier leg delivered less than
/// its quote). From the program's on-chain IDL, read 2026-09-22.
const JUP_INSUFFICIENT_FUNDS: i64 = 6024;
const JUPITER_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";

pub fn classify(err: &Value, logs: &[String]) -> SimFailureClass {
    let e = err.to_string();
    let l = logs.join("\n");
    let has = |s: &str| e.contains(s) || l.contains(s);
    if has("SlippageToleranceExceeded") || custom_code(err) == Some(JUP_SLIPPAGE) || l.contains("0x1771") {
        SimFailureClass::SlippageExceeded
    } else if has("InsufficientFunds")
        || has("insufficient funds")
        || has("insufficient lamports")
        || custom_code(err) == Some(JUP_INSUFFICIENT_FUNDS)
    {
        SimFailureClass::InsufficientFunds
    } else if has("BlockhashNotFound") {
        SimFailureClass::BlockhashNotFound
    } else if has("AccountNotFound")
        || has("ProgramAccountNotFound")
        || has("InvalidAccountForFee")
        || has("IncorrectProgramId")
        || has("UninitializedAccount")
    {
        SimFailureClass::AccountNotFound
    } else if has("ComputationalBudgetExceeded") || has("exceeded CUs meter") || has("ProgramFailedToComplete") {
        SimFailureClass::ComputeExceeded
    } else if has("TooManyAccountLocks") || has("MaxLoadedAccountsDataSizeExceeded") {
        SimFailureClass::TooManyAccounts
    } else if has("InstructionError") {
        SimFailureClass::ProgramError
    } else {
        SimFailureClass::Unknown
    }
}

fn custom_code(err: &Value) -> Option<i64> {
    // {"InstructionError":[idx,{"Custom":code}]}
    err.get("InstructionError")?.get(1)?.get("Custom")?.as_i64()
}

pub fn failure_from(index: u8, out: &SimulateOutcome) -> Option<SimFailure> {
    let err = out.err.as_ref()?;
    let class = classify(err, &out.logs);
    // Most informative log line: the last "Error"/"failed" line if any.
    let log_hint = out
        .logs
        .iter()
        .rev()
        .find(|l| l.contains("Error") || l.contains("failed") || l.contains("insufficient"))
        .cloned()
        .unwrap_or_default();
    let message = if log_hint.is_empty() { err.to_string() } else { format!("{err} · {log_hint}") };
    Some(SimFailure { class, tx_index: index, message })
}

/// Output amounts returned by top-level Jupiter route instructions, in order.
pub fn jupiter_outputs(logs: &[String]) -> Vec<u64> {
    let prefix = format!("Program return: {JUPITER_PROGRAM} ");
    logs.iter()
        .filter_map(|l| l.strip_prefix(&prefix))
        .filter_map(|b64| base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok())
        .filter_map(|b| Some(u64::from_le_bytes(b.get(..8)?.try_into().ok()?)))
        .collect()
}

/// Accounts with no lamports before and some after: created by the
/// transaction and left open, funded by the payer.
pub fn created_accounts(keys: &[Address], pre: &[u64], post: &[u64]) -> Vec<(Address, u64)> {
    keys.iter()
        .zip(pre.iter().zip(post))
        .filter(|(_, (b, a))| **b == 0 && **a > 0)
        .map(|(k, (_, a))| (*k, *a))
        .collect()
}

/// `keys`: the transaction's account keys in message order (static + lookup
/// table), when known; needed to name created accounts.
pub fn tx_sim(index: u8, a: &AssembledTx, out: &SimulateOutcome, margin: Ppm, keys: Option<&[Address]>) -> TxSim {
    let used = out.units_consumed.unwrap_or(0).min(u32::MAX as u64) as u32;
    let taker = match (&out.pre_balances, &out.post_balances) {
        (Some(pre), Some(post)) if !pre.is_empty() && !post.is_empty() => Some((pre[0], post[0])),
        _ => None,
    };
    let created = match (keys, &out.pre_balances, &out.post_balances) {
        (Some(k), Some(pre), Some(post)) if out.err.is_none() && k.len() == pre.len() => created_accounts(k, pre, post),
        _ => Vec::new(),
    };
    TxSim {
        index,
        ok: out.err.is_none(),
        units_consumed: used,
        cu_limit: if used > 0 { cu_limit_with_margin(used, margin) } else { a.cu_limit },
        cu_price_micro: a.cu_price_micro,
        fee: out.fee,
        size_bytes: a.size as u32,
        accounts: a.accounts as u16,
        logs: out.logs.clone(),
        err: out.err.as_ref().map(|e| e.to_string()),
        taker_lamports: taker,
        leg_outputs: jupiter_outputs(&out.logs),
        created,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classification() {
        let c = |e: Value, logs: &[&str]| classify(&e, &logs.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(c(json!({"InstructionError":[3,{"Custom":6001}]}), &[]), SimFailureClass::SlippageExceeded);
        assert_eq!(
            c(json!({"InstructionError":[3,{"Custom":1}]}), &["Program log: Error: insufficient funds"]),
            SimFailureClass::InsufficientFunds
        );
        assert_eq!(c(json!("InsufficientFundsForFee"), &[]), SimFailureClass::InsufficientFunds);
        assert_eq!(c(json!("BlockhashNotFound"), &[]), SimFailureClass::BlockhashNotFound);
        assert_eq!(c(json!("AccountNotFound"), &[]), SimFailureClass::AccountNotFound);
        assert_eq!(
            c(json!({"InstructionError":[3,"IncorrectProgramId"]}), &["Program log: Error: IncorrectProgramId"]),
            SimFailureClass::AccountNotFound,
            "token op on a closed/uninitialised account"
        );
        assert_eq!(
            c(json!({"InstructionError":[2,"ComputationalBudgetExceeded"]}), &[]),
            SimFailureClass::ComputeExceeded
        );
        assert_eq!(c(json!("TooManyAccountLocks"), &[]), SimFailureClass::TooManyAccounts);
        assert_eq!(c(json!({"InstructionError":[1,"InvalidAccountData"]}), &[]), SimFailureClass::ProgramError);
        assert_eq!(c(json!("SomethingNew"), &[]), SimFailureClass::Unknown);
    }

    #[test]
    fn jupiter_6024_is_insufficient_funds() {
        let logs = vec![
            "Program JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4 failed: custom program error: 0x1788".to_string(),
        ];
        assert_eq!(
            classify(&json!({"InstructionError":[9,{"Custom":6024}]}), &logs),
            SimFailureClass::InsufficientFunds
        );
    }

    #[test]
    fn leg_outputs_and_created_accounts_from_a_recorded_simulation() {
        // lines from simulation 20260920-103123-50a6 #12450 (Raydium CLMM → HumidiFi)
        let logs: Vec<String> = [
            "Program return: TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA pQAAAAAAAAA=",
            "Program return: JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4 f4OwAAAAAAA=",
            "Program return: JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4 6L/0BQAAAAA=",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(jupiter_outputs(&logs), vec![11_567_999, 99_925_992]);
        let (a, b, c) = (Address([1; 32]), Address([2; 32]), Address([3; 32]));
        // a: payer (debited), b: created and kept (13,045,440 = rent of 2,440 bytes), c: created and closed
        let got = created_accounts(&[a, b, c], &[136_849_918, 0, 0], &[123_721_514, 13_045_440, 0]);
        assert_eq!(got, vec![(b, 13_045_440)]);
    }

    #[test]
    fn failure_message_prefers_error_log() {
        let out = SimulateOutcome {
            err: Some(json!({"InstructionError":[3,{"Custom":6001}]})),
            logs: vec!["Program JUP6 invoke [1]".into(), "Program log: Error: SlippageToleranceExceeded".into()],
            ..Default::default()
        };
        let f = failure_from(0, &out).unwrap();
        assert_eq!(f.class, SimFailureClass::SlippageExceeded);
        assert!(f.message.contains("SlippageToleranceExceeded"));
        assert!(failure_from(0, &SimulateOutcome::default()).is_none());
    }
}
