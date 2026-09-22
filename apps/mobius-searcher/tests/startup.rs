//! PAPER must start without any private key; sending modes must refuse to
//! start unless explicitly gated. Endpoints point at a closed local port so
//! these tests never touch the network.

use searcher_core::config::Config;

fn offline_config(extra: &str) -> Config {
    let toml = format!(
        r#"
[jupiter]
base_url = "http://127.0.0.1:9"
price_refresh_ms = 0
[rpc]
url = "http://127.0.0.1:9"
url_env = "SEARCHER_TEST_NO_SUCH_VAR"
ws_url = "ws://127.0.0.1:9"
ws_url_env = "SEARCHER_TEST_NO_SUCH_VAR"
[jito]
block_engine_url = "http://127.0.0.1:9"
tip_floor_url = "http://127.0.0.1:9/tip_floor"
{extra}
"#
    );
    Config::from_toml(&toml).expect("config")
}

fn db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("mobius-startup-{}-{}.sqlite", std::process::id(), rand_suffix()))
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paper_starts_without_any_private_key_and_stops_cleanly() {
    // No wallet section at all, no keypair anywhere.
    let cfg = offline_config("");
    assert!(cfg.wallet.keypair_path.is_none());
    let path = db();
    let running = mobius_searcher::engine::start(cfg, path.clone()).await.expect("PAPER must start without a key");
    let id = running.session_id.clone();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let stats = running.stop().await;
    assert!(stats.written >= 1, "session event recorded");
    let store = searcher_storage::Store::open(&path).unwrap();
    let s = store.list_sessions().unwrap().into_iter().find(|s| s.id == id).expect("session row");
    assert_eq!(s.mode, "PAPER");
    assert!(s.ended_at.is_some(), "session closed on shutdown");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn live_and_confirm_refuse_to_start_without_explicit_gate() {
    let e = Config::from_toml("[general]\nmode = \"live\"\n[wallet]\nkeypair_path = \"/k.json\"\n").unwrap_err();
    assert!(e.to_string().contains("live_enabled"));
    let e = Config::from_toml("[general]\nmode = \"confirm\"\n").unwrap_err();
    assert!(e.to_string().contains("live_enabled"));
    // A private key in the environment never enables sending on its own.
    unsafe { std::env::set_var("SOLANA_PRIVATE_KEY", "not-used") };
    let c = Config::from_toml("").unwrap();
    assert_eq!(c.general.mode, searcher_core::Mode::Paper);
    assert!(!c.execution.live_enabled);
}

#[tokio::test]
async fn live_gate_with_missing_keypair_fails_before_any_network_use() {
    let cfg = offline_config(
        "[general]\nmode = \"live\"\n[execution]\nlive_enabled = true\n[wallet]\nkeypair_path = \"/definitely/not/here.json\"\n",
    );
    let err = mobius_searcher::engine::start(cfg, db()).await.err().expect("must fail");
    assert!(format!("{err:#}").contains("hot wallet"), "{err:#}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_threshold_changes_apply_live_and_are_saved_with_comments_kept() {
    use searcher_core::event::Command;
    let dir = std::env::temp_dir().join(format!("mobius-thr-e2e-{}", rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let user = dir.join("config.toml");
    std::fs::write(&user, "# mine\n[profit]\nsafety_buffer_lamports = 5000   # keep this note\n").unwrap();
    let running =
        mobius_searcher::engine::start_with(offline_config(""), db(), Some(user.clone())).await.expect("start");
    let wait = || tokio::time::sleep(std::time::Duration::from_millis(400));
    wait().await;
    assert!(!running.vm.read().thresholds.is_empty(), "thresholds published at start");

    // loosening into losses without the acknowledgement: refused, nothing written
    running
        .commands
        .send(Command::SetThresholds {
            changes: vec![("profit.protect_min_out".into(), "false".into())],
            allow_loss: false,
        })
        .unwrap();
    wait().await;
    assert!(!running.vm.read().loss_possible);
    assert!(std::fs::read_to_string(&user).unwrap().contains("safety_buffer_lamports = 5000"));

    // a safe change plus the acknowledged one
    running
        .commands
        .send(Command::SetThresholds {
            changes: vec![
                ("profit.safety_buffer_lamports".into(), "0".into()),
                ("profit.protect_min_out".into(), "false".into()),
            ],
            allow_loss: true,
        })
        .unwrap();
    wait().await;
    {
        let vm = running.vm.read();
        assert!(vm.loss_possible);
        assert!(vm.thresholds.contains(&("profit.safety_buffer_lamports".into(), "0".into())));
        assert!(vm.logs.iter().any(|l| l.message.contains("threshold profit.safety_buffer_lamports: 5000 → 0")));
    }
    let saved = std::fs::read_to_string(&user).unwrap();
    assert!(saved.contains("safety_buffer_lamports = 0   # keep this note"), "{saved}");
    assert!(saved.contains("protect_min_out = false") && saved.contains("# mine"), "{saved}");
    assert!(dir.join("config.toml.bak").exists());
    running.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}
