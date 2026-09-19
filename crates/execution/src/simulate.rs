//! Simulation result interpretation and failure classification.

use crate::assemble::AssembledTx;
use searcher_core::Ppm;
use searcher_core::model::{SimFailure, SimFailureClass, TxSim};
use searcher_core::units::cu_limit_with_margin;
use searcher_market::SimulateOutcome;
use serde_json::Value;

/// Jupiter aggregator `SlippageToleranceExceeded` custom error (0x1771).
const JUP_SLIPPAGE: i64 = 6001;

pub fn classify(err: &Value, logs: &[String]) -> SimFailureClass {
    let e = err.to_string();
    let l = logs.join("\n");
    let has = |s: &str| e.contains(s) || l.contains(s);
    if has("SlippageToleranceExceeded") || custom_code(err) == Some(JUP_SLIPPAGE) || l.contains("0x1771") {
        SimFailureClass::SlippageExceeded
    } else if has("InsufficientFunds") || has("insufficient funds") || has("insufficient lamports") {
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

pub fn tx_sim(index: u8, a: &AssembledTx, out: &SimulateOutcome, margin: Ppm) -> TxSim {
    let used = out.units_consumed.unwrap_or(0).min(u32::MAX as u64) as u32;
    let taker = match (&out.pre_balances, &out.post_balances) {
        (Some(pre), Some(post)) if !pre.is_empty() && !post.is_empty() => Some((pre[0], post[0])),
        _ => None,
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
