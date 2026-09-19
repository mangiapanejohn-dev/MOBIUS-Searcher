//! Live probe: build a dex-constrained SOL→USDC→SOL cycle on Jupiter V2,
//! assemble one v0 tx, simulate on mainnet (sigVerify=false). Nothing is
//! signed or sent. Usage:
//! `JUPITER_API_KEY=… cargo run -p searcher-execution --example probe_sim -- <taker_pubkey>`

use searcher_core::Address;
use searcher_core::address::well_known;
use searcher_core::model::{DexFilter, RoutingMode, SlippageSpec};
use searcher_execution::assemble::{AssemblyParams, ata_create_target, compose_single};
use searcher_jupiter::{ApiKey, BuildRequest, JupiterClient};
use searcher_market::RpcClient;
use searcher_telemetry::{LimiterConfig, Telemetry};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let taker: Address = std::env::args().nth(1).expect("taker pubkey").parse().expect("pubkey");
    let tel = Arc::new(Telemetry::new());
    let jup = JupiterClient::new(
        "https://api.jup.ag",
        std::env::var("JUPITER_API_KEY").ok().and_then(ApiKey::new),
        LimiterConfig::new(0.9, 1),
        Duration::from_secs(6),
        tel.clone(),
    )
    .unwrap();
    let rpc = RpcClient::new(
        &std::env::var("SOLANA_RPC_URL").unwrap_or("https://api.mainnet-beta.solana.com".into()),
        LimiterConfig::new(2.0, 2),
        1.0,
        Duration::from_secs(8),
        tel,
    )
    .unwrap();
    let sol = well_known::addr(well_known::WSOL_MINT);
    let usdc = well_known::addr(well_known::USDC_MINT);
    let mk = |i, o, amount, dex: &str| BuildRequest {
        input_mint: i,
        output_mint: o,
        amount,
        taker,
        slippage: SlippageSpec::Rtse,
        mode: RoutingMode::Fast,
        dex_filter: DexFilter::Only(vec![dex.into()]),
        cu_price_percentile: "high".into(),
        max_accounts: Some(30),
        blockhash_slots_to_expiry: 150,
        for_jito_bundle: true,
    };
    let l1 = jup.build(&mk(sol, usdc, 1_000_000_000, "Raydium CLMM"), 0).await.expect("leg1");
    let l2 = jup.build(&mk(usdc, sol, l1.leg.out_amount, "Whirlpool"), 1).await.expect("leg2");
    println!("leg1 {} → {} ({:?}) {}ms", l1.leg.in_amount, l1.leg.out_amount, l1.leg.dex_labels(), l1.leg.latency_ms);
    println!("leg2 {} → {} ({:?}) {}ms", l2.leg.in_amount, l2.leg.out_amount, l2.leg.dex_labels(), l2.leg.latency_ms);
    println!("gross pnl {} lamports", l2.leg.out_amount as i64 - 1_000_000_000);

    let atas: Vec<Address> =
        l1.instructions.setup.iter().chain(&l2.instructions.setup).filter_map(ata_create_target).collect();
    let exist = rpc.get_accounts_lamports(&atas).await.expect("atas");
    let existing: HashSet<Address> = atas.iter().zip(&exist).filter(|(_, e)| e.is_some()).map(|(a, _)| *a).collect();
    println!("ATAs: {:?}", atas.iter().zip(&exist).map(|(a, e)| (a.short(), e.is_some())).collect::<Vec<_>>());

    let tip: Address = searcher_execution::KNOWN_TIP_ACCOUNT.parse().unwrap();
    let p = AssemblyParams {
        payer: taker,
        cu_limit: 1_400_000,
        cu_price_micro: l1.leg.cu_price_micro.unwrap_or(0),
        tip: Some((tip, 10_000)),
        dont_front: None,
        existing_atas: &existing,
        blockhash: l1.instructions.blockhash,
    };
    let a = compose_single(&[&l1.instructions, &l2.instructions], &p).expect("compose");
    println!("tx: {} bytes, {} account locks, creates {:?}", a.size, a.accounts, a.creates_atas);
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&a.wire);
    let wsol_ata = atas.first().copied();
    let watch: Vec<Address> = std::iter::once(taker).chain(wsol_ata).collect();
    let out = rpc.simulate(&b64, &watch).await.expect("simulate");
    println!("slot {:?} err {:?}", out.context_slot, out.err);
    println!("units {:?} fee {:?}", out.units_consumed, out.fee);
    println!("pre[0..3] {:?}", out.pre_balances.as_ref().map(|v| v.iter().take(3).collect::<Vec<_>>()));
    println!("post[0..3] {:?}", out.post_balances.as_ref().map(|v| v.iter().take(3).collect::<Vec<_>>()));
    println!("watched post lamports {:?}", out.post_account_lamports);
    if let (Some(pre), Some(post)) = (&out.pre_balances, &out.post_balances) {
        let legs = [&l1.instructions, &l2.instructions];
        let keys = searcher_execution::assemble::message_account_keys(&a.tx, &legs).expect("keys");
        let wsol = searcher_execution::assemble::wsol_accounts(&legs);
        println!("keys {} balances {}", keys.len(), pre.len());
        for (i, k) in keys.iter().enumerate() {
            if pre.get(i) != post.get(i) {
                println!("  Δ {} {:>16} → {:>16}  {:+}", k.short(), pre[i], post[i], post[i] as i64 - pre[i] as i64);
            }
        }
        println!(
            "native taker delta {} | SOL-equivalent delta {:?} | quoted gross {} | fee {:?} tip 10000",
            post[0] as i64 - pre[0] as i64,
            searcher_execution::assemble::sol_equivalent_delta(&keys, &taker, &wsol, pre, post),
            l2.leg.out_amount as i64 - 1_000_000_000,
            out.fee
        );
    }
    for l in out.logs.iter().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
        println!("  | {l}");
    }
    println!("latency {} ms", out.latency_ms);
}
