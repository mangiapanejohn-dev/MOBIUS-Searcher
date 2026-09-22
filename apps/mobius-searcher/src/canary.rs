//! `--canary`: one real trade through the LIVE path, to prove it works end to
//! end before anything else is trusted with money.
//!
//! It is the normal engine in CONFIRM mode (every candidate waits for `y`)
//! with one route (SOL → USDC → SOL) and the profit guards replaced by a loss
//! bound: `min_profit_lamports = −max_loss`, which pricing also writes into
//! the final leg's on-chain minimum output. After the first landed trade the
//! session ends and the transaction is reconciled account by account.

use anyhow::{Result, bail};
use searcher_core::config::{Config, RoundTripConfig, SchedulerKind};
use searcher_core::model::{Mode, Opportunity};
use searcher_core::{Address, address::well_known};
use searcher_market::TxMeta;
use serde::Serialize;

/// The configuration a canary session runs with.
pub fn prepare(cfg: &Config) -> Result<Config> {
    if !cfg.execution.live_enabled || cfg.wallet.keypair_path.is_none() {
        bail!(
            "--canary sends one real transaction: it needs execution.live_enabled = true and \
             [wallet] keypair_path in your config (run --setup, path Assisted or Advanced)"
        );
    }
    let mut c = cfg.clone();
    let max_loss = c.canary.max_loss_lamports;
    let amount = match c.canary.amount_lamports {
        0 => c
            .strategies
            .round_trip
            .iter()
            .find(|s| s.enabled && s.base == "SOL" && s.quote == "USDC")
            .map(|s| s.amount_lamports)
            .unwrap_or(100_000_000),
        a => a,
    };
    c.general.mode = Mode::Confirm;
    c.paper.simulation_taker = None;
    c.profit.min_profit_lamports = -(max_loss.min(i64::MAX as u64) as i64);
    c.profit.min_profit_bps = -10_000;
    c.profit.min_profit_usd = "-1000000".into();
    c.profit.protect_min_out = true;
    c.strategies.round_trip = vec![RoundTripConfig { amount_lamports: amount, ..RoundTripConfig::default() }];
    c.strategies.cross_dex.iter_mut().for_each(|s| s.enabled = false);
    c.strategies.triangular.iter_mut().for_each(|s| s.enabled = false);
    c.scheduler.kind = SchedulerKind::RoundRobin;
    c.validate().map_err(anyhow::Error::msg)?;
    Ok(c)
}

#[derive(Clone, Debug, Serialize)]
pub struct Line {
    pub what: String,
    /// What the recorded quotes / model said.
    pub model: String,
    /// What the chain recorded.
    pub chain: String,
    pub ok: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Reconciliation {
    pub signature: String,
    pub slot: u64,
    pub lines: Vec<Line>,
    /// SOL-equivalent result of the trade (native + USDC at the executed
    /// rate + deposits returned as capital), lamports.
    pub sol_equivalent: i64,
    pub max_loss: u64,
    pub ok: bool,
}

fn token_delta(tx: &TxMeta, owner: &Address, mint: &Address) -> i128 {
    let sum = |v: &[searcher_market::TokenBalance]| -> i128 {
        v.iter().filter(|t| &t.mint == mint && t.owner.as_ref() == Some(owner)).map(|t| t.amount as i128).sum()
    };
    sum(&tx.post_tokens) - sum(&tx.pre_tokens)
}

/// Explain every lamport the taker's wallet moved by the executed outputs,
/// the fee, the tip and the deposits; compare with the quotes.
pub fn reconcile(
    opp: &Opportunity,
    taker: &Address,
    usdc: &Address,
    tip: u64,
    max_loss: u64,
    signature: &str,
    tx: &TxMeta,
) -> Reconciliation {
    let mut lines = Vec::new();
    let mut line = |what: &str, model: String, chain: String, ok: bool| {
        lines.push(Line { what: what.into(), model, chain, ok });
    };
    line("transaction succeeded", "yes".into(), tx.err.clone().unwrap_or_else(|| "yes".into()), tx.err.is_none());

    let legs = &opp.route.legs;
    let executed = searcher_execution::simulate::jupiter_outputs(&tx.logs);
    for (i, l) in legs.iter().enumerate() {
        let got = executed.get(i).copied();
        line(
            &format!("leg {} output (quoted → executed)", i + 1),
            l.out_amount.to_string(),
            got.map(|g| g.to_string()).unwrap_or_else(|| "missing".into()),
            got.is_some_and(|g| g >= l.min_out),
        );
    }

    // accounts created by the transaction and left open: deposits (capital)
    let created: u64 = tx.pre.iter().zip(&tx.post).filter(|(b, a)| **b == 0 && **a > 0).map(|(_, a)| *a).sum();
    line(
        "deposits (accounts left created)",
        opp.costs.ata_rent.to_string(),
        created.to_string(),
        created == opp.costs.ata_rent,
    );

    // native SOL of the taker: −input + final output − fee − tip − deposits (wSOL is wrapped and closed in-tx)
    let wsol = well_known::addr(well_known::WSOL_MINT);
    let taker_i = tx.keys.iter().position(|k| k == taker);
    let native = taker_i.and_then(|i| Some(*tx.post.get(i)? as i128 - *tx.pre.get(i)? as i128));
    let wsol_tokens = token_delta(tx, taker, &wsol);
    let expected_native = match (executed.last(), legs.first()) {
        (Some(out), Some(first)) if executed.len() == legs.len() => {
            Some(-(first.in_amount as i128) + *out as i128 - tx.fee as i128 - tip as i128 - created as i128)
        }
        _ => None,
    };
    let chain_sol = native.map(|n| n + wsol_tokens);
    line(
        "taker SOL change (explained by outputs, fee, tip, deposits)",
        expected_native.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        chain_sol.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        expected_native.is_some() && expected_native == chain_sol,
    );

    // USDC: what leg 1 delivered minus what leg 2 spent (the inventory drift)
    let usdc_chain = token_delta(tx, taker, usdc);
    let usdc_model = match (executed.first(), legs.get(1)) {
        (Some(o1), Some(l2)) => Some(*o1 as i128 - l2.in_amount as i128),
        _ => None,
    };
    line(
        "taker USDC change (leg 1 executed − leg 2 input)",
        usdc_model.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        usdc_chain.to_string(),
        usdc_model == Some(usdc_chain),
    );

    // SOL-equivalent result: native + deposits (capital) + USDC drift at leg 2's executed rate
    let drift_sol = match (legs.get(1), executed.get(1)) {
        (Some(l2), Some(o2)) if l2.in_amount > 0 => usdc_chain * *o2 as i128 / l2.in_amount as i128,
        _ => 0,
    };
    let sol_equivalent = (chain_sol.unwrap_or(0) + created as i128 + drift_sol) as i64;
    line(
        "result within the loss bound",
        format!("≥ −{max_loss}"),
        sol_equivalent.to_string(),
        sol_equivalent >= -(max_loss as i64),
    );
    line(
        "fee (model base + priority → chain)",
        (opp.costs.base_fee + opp.costs.priority_fee).to_string(),
        tx.fee.to_string(),
        true, // informative: the model priced the priority fee from the simulated CU
    );
    let ok = lines.iter().all(|l| l.ok);
    Reconciliation { signature: signature.into(), slot: tx.slot, lines, sol_equivalent, max_loss, ok }
}

pub fn render(r: &Reconciliation) -> String {
    let mut o = format!(
        "CANARY RECONCILIATION · {} · slot {}\nhttps://solscan.io/tx/{}\n\n",
        if r.ok { "ALL LINES MATCH" } else { "MISMATCH — do not trust the ledger until explained" },
        r.slot,
        r.signature
    );
    for l in &r.lines {
        o.push_str(&format!(
            "  {} {:<58} model {:>14}   chain {:>14}\n",
            if l.ok { "ok " } else { "!! " },
            l.what,
            l.model,
            l.chain
        ));
    }
    o.push_str(&format!(
        "\n  SOL-equivalent result: {:+} lamports ({:+.6} SOL); bound −{} lamports\n",
        r.sol_equivalent,
        r.sol_equivalent as f64 / 1e9,
        r.max_loss
    ));
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use searcher_core::model::*;
    use searcher_core::{Ppm, Ts};
    use searcher_market::TokenBalance;

    fn leg(i: u8, input: Address, output: Address, ina: u64, out: u64) -> Leg {
        Leg {
            index: i,
            input_mint: input,
            output_mint: output,
            in_amount: ina,
            out_amount: out,
            min_out: out - out / 1_000,
            slippage_bps: 10,
            slippage_spec: SlippageSpec::Fixed(10),
            price_impact: Ppm::ZERO,
            hops: vec![],
            mode: RoutingMode::Normal,
            dex_filter: DexFilter::Any,
            quoted_at: Ts(0),
            latency_ms: 1,
            cu_price_micro: None,
            last_valid_block_height: 0,
            request_id: None,
        }
    }

    #[test]
    fn prepare_refuses_without_the_live_gate_and_bounds_the_loss() {
        assert!(prepare(&Config::default()).unwrap_err().to_string().contains("live_enabled"));
        let mut c = Config::default();
        c.general.mode = Mode::Live;
        c.execution.live_enabled = true;
        c.wallet.keypair_path = Some("/k.json".into());
        let p = prepare(&c).unwrap();
        assert_eq!(p.general.mode, Mode::Confirm, "every candidate waits for y");
        assert_eq!(p.profit.min_profit_lamports, -500_000);
        assert!(p.profit.protect_min_out);
        assert_eq!(p.strategies.round_trip.len(), 1);
        assert!(p.strategies.cross_dex.iter().all(|s| !s.enabled));
    }

    #[test]
    fn a_landed_round_trip_reconciles_to_the_lamport() {
        let taker = Address([7; 32]);
        let sol = well_known::addr(well_known::WSOL_MINT);
        let usdc = Address([2; 32]);
        // quotes: 0.1 SOL → 11,568,000 USDC atoms → 99,930,000 lamports
        let legs = vec![leg(0, sol, usdc, 100_000_000, 11_568_000), leg(1, usdc, sol, 11_568_000, 99_930_000)];
        let opp = Opportunity {
            id: OpportunityId(1),
            key: "rt".into(),
            strategy: StrategyKind::RoundTrip,
            label: "SOL→USDC→SOL".into(),
            detected_at: Ts(0),
            slot: None,
            base_mint: sol,
            input: 100_000_000,
            gross_output: 99_930_000,
            route: Route { legs },
            costs: searcher_core::costs::CostBreakdown { base_fee: 5_000, priority_fee: 1_900, ..Default::default() },
            eval: Default::default(),
            status: OppStatus::Landed,
            updated_at: Ts(0),
            sol_price: None,
            simulation: None,
            risk: None,
            guard: None,
        };
        // chain: leg 1 delivered 1 atom less (from inventory), leg 2 returned 99,925,992
        let fee = 7_956u64;
        let tip = 1_000u64;
        let native_delta: i64 = -100_000_000 + 99_925_992 - fee as i64 - tip as i64;
        let tx = TxMeta {
            slot: 9,
            err: None,
            fee,
            keys: vec![taker, Address([3; 32])],
            pre: vec![136_849_918, 5],
            post: vec![(136_849_918 + native_delta) as u64, 5],
            pre_tokens: vec![TokenBalance { index: 1, mint: usdc, owner: Some(taker), amount: 5_000_000 }],
            post_tokens: vec![TokenBalance { index: 1, mint: usdc, owner: Some(taker), amount: 4_999_999 }],
            logs: vec![
                "Program return: JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4 f4OwAAAAAAA=".into(), // 11,567,999
                "Program return: JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4 6L/0BQAAAAA=".into(), // 99,925,992
            ],
        };
        let r = reconcile(&opp, &taker, &usdc, tip, 500_000, "sig", &tx);
        assert!(r.ok, "{}", render(&r));
        assert_eq!(r.sol_equivalent, native_delta - 8, "one USDC atom ≈ 8 lamports of drift");
        // a lamport nobody can explain is a mismatch
        let mut bad = tx.clone();
        bad.post[0] -= 1;
        assert!(!reconcile(&opp, &taker, &usdc, tip, 500_000, "sig", &bad).ok);
    }
}
