//! The bots as the TUI's Bots page shows them, and its three actions on a
//! real one. Everything is read from what the runs wrote down
//! (`research.sqlite`, the lock of `--trade`); nothing here decides or sends
//! anything. Starting a bot starts `--trade FILE` as a program of its own, so
//! it goes on when the window that started it is closed; stopping it is the
//! Ctrl-C it would get in its own window; closing it is `--trade FILE --close`.

use super::rules::{Account, Bar};
use super::{Experiment, Plan, parse, trade};
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use searcher_core::config::Config;
use searcher_storage::ResearchStore;
use searcher_tui::bots::{BotAction, BotPort, BotState, BotView, BotsView, Calc, Levels, NewBot};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Ended real runs shown beside the ones that are not.
const ENDED_SHOWN: usize = 2;
/// Bars read for the picture when the rule's own window is shorter.
const BARS_SHOWN: usize = 200;

/// Who holds the lock of `--trade`, written beside it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Holder {
    pub pid: u32,
    pub run: String,
    pub file: String,
    /// It reads wishes to change its budget (a version before this one did not).
    #[serde(default)]
    pub wishes: bool,
}

fn holder_path(data_dir: &Path) -> PathBuf {
    data_dir.join("trade.holder")
}

/// Said by `--trade` once it holds the lock: which run, from which file.
pub fn hold(data_dir: &Path, run: &str, file: &Path) -> Result<()> {
    let file = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let holder =
        Holder { pid: std::process::id(), run: run.to_string(), file: file.display().to_string(), wishes: true };
    std::fs::write(holder_path(data_dir), serde_json::to_string(&holder)?)?;
    // the file of a run is remembered after it stopped, to start it again
    std::fs::write(data_dir.join(format!("{run}.path")), holder.file.as_bytes())?;
    Ok(())
}

/// Where a wish to change a run's budget waits for its program: a number of USD.
pub fn wish_path(data_dir: &Path, run: &str) -> PathBuf {
    data_dir.join(format!("{run}.budget"))
}

/// The wish left at `path`, when one is.
pub fn wish(path: &Path) -> Option<f64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok().filter(|v: &f64| v.is_finite())
}

/// Whether a `--trade` runs now (its lock is held), and what it said of itself.
fn running(data_dir: &Path) -> (bool, Option<Holder>) {
    let Ok(lock) = std::fs::OpenOptions::new().read(true).write(true).open(data_dir.join("trade.lock")) else {
        return (false, None);
    };
    if lock.try_lock().is_ok() {
        return (false, None); // free: released again as `lock` goes
    }
    let said = std::fs::read_to_string(holder_path(data_dir)).ok().and_then(|s| serde_json::from_str(&s).ok());
    (true, said)
}

/// The rules file of a run: the one it was last started from, else a file of
/// `rules_dir` whose content is this run's.
fn file_of(data_dir: &Path, rules_dir: &Path, run: &str) -> Option<PathBuf> {
    let names = |p: &Path| std::fs::read_to_string(p).ok().and_then(|t| parse(&t).ok()).map(|plan| plan.id);
    let is_it = |p: &Path| names(p).is_some_and(|id| format!("trade-{id}") == run);
    if let Ok(p) = std::fs::read_to_string(data_dir.join(format!("{run}.path"))).map(PathBuf::from)
        && is_it(&p)
    {
        return Some(p);
    }
    rules_files(rules_dir).into_iter().find(|p| is_it(p))
}

/// The `.toml` files of the directory where rules files are kept.
fn rules_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flat_map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect();
    files.sort();
    files
}

/// Where the bots are read from and acted on.
struct Desk {
    data_dir: PathBuf,
    /// Where rules files are kept (the config directory): one that has not run yet is found there.
    rules_dir: PathBuf,
    /// The `--config` this program was started with: a bot it starts gets the same.
    config_file: Option<PathBuf>,
    /// The operator reads Chinese: what is said to them is said in it.
    zh: bool,
    live_enabled: bool,
    store: Mutex<Option<ResearchStore>>,
}

fn bars_of(store: &ResearchStore, plan: &Plan, want: usize) -> Result<Vec<Bar>> {
    let from = searcher_core::Ts::now().millis() - (want as i64 + 2) * plan.bar_ms;
    let rows = store.lab_bars(&plan.instrument, &plan.bar, from)?;
    Ok(rows.iter().map(|b| Bar { ts: b.0, open: b.1, high: b.2, low: b.3, close: b.4, volume: b.5 }).collect())
}

/// What is common to a paper account and a real one.
fn bot(plan: &Plan, e: &Experiment, acct: &Account, bars: &[Bar], zh: bool) -> BotView {
    let last = bars.last().map(|b| b.close);
    let (buy, sell, stop) = e.rule.levels(bars, acct);
    let shown = &bars[bars.len().saturating_sub(BARS_SHOWN)..];
    let mut fills: Vec<(i64, bool)> = acct.lots.iter().map(|l| (l.opened, true)).collect();
    fills.extend(acct.trades.iter().flat_map(|t| [(t.opened, true), (t.closed, false)]));
    BotView {
        id: String::new(),
        name: e.name.clone(),
        real: false,
        inst: plan.instrument.clone(),
        bar: plan.bar.clone(),
        bar_ms: plan.bar_ms,
        rule: e.rule.describe(plan.bar_ms, zh),
        state: BotState::Paper,
        funded: true,
        pending: false,
        budget: plan.capital,
        stop_at: None,
        cash: acct.cash,
        sol: acct.sol(),
        paid: acct.lots.iter().map(|l| l.usd).sum(),
        worth: last.map(|p| acct.cash + acct.sol() * p),
        trades: acct.trades.iter().map(|t| (t.closed, t.usd, t.net)).collect(),
        levels: Levels { buy, sell, stop },
        calc: e.rule.dip(bars).map(|(window, mean, sd, k, exit_z, stop)| Calc { window, mean, sd, k, exit_z, stop }),
        closes: shown.iter().map(|b| (b.ts + plan.bar_ms, b.close)).collect(),
        fills,
        journal: Vec::new(),
        file: None,
        wish: None,
        opened: acct.lots.first().map(|l| l.opened),
        equity: Vec::new(),
    }
}

impl Desk {
    fn read(&self) -> Result<BotsView> {
        // no records yet is not an error, and no reason to make the file: a rules file can still be listed
        let db = self.data_dir.join("research.sqlite");
        let mut guard = self.store.lock();
        if guard.is_none() && db.exists() {
            *guard = Some(ResearchStore::open(&db).with_context(|| format!("opening {}", db.display()))?);
        }
        let store = guard.as_ref();
        let bars_for = |plan: &Plan, want: usize| store.map_or(Ok(Vec::new()), |s| bars_of(s, plan, want));
        let (is_running, holder) = running(&self.data_dir);
        let mut runs = store.map_or(Ok(Vec::new()), |s| s.lab_runs())?;
        runs.reverse(); // newest first
        let (mut bots, mut ended) = (Vec::new(), 0);
        // a run started by a version that did not say which it is: the newest that has not ended
        let mut unnamed = is_running && holder.is_none();
        for (id, _, manifest) in runs.iter().filter(|r| r.0.starts_with("trade-")) {
            let Ok(plan) = parse(manifest) else { continue };
            let (Some(live), [e]) = (&plan.live, plan.experiments.as_slice()) else { continue };
            let Some(store) = store else { continue };
            let state: trade::State = match store.lab_state(id, &e.name)? {
                Some((_, json)) => serde_json::from_str(&json).with_context(|| format!("the state of {id}"))?,
                None => trade::State::default(),
            };
            if state.ended.is_some() {
                ended += 1;
                if ended > ENDED_SHOWN {
                    continue;
                }
            }
            let window = e.rule.warmup().max(BARS_SHOWN);
            let bars = bars_for(&plan, window)?;
            let mut b = bot(&plan, e, &state.account, &bars, self.zh);
            let by_name = holder.as_ref().is_some_and(|h| h.run == *id);
            let runs_now = state.ended.is_none() && (by_name || std::mem::take(&mut unnamed));
            b.state = match &state.ended {
                Some(why) => BotState::Ended(why.clone()),
                None if runs_now => BotState::Running,
                None => BotState::Stopped,
            };
            (b.id, b.real, b.funded, b.pending) = (id.clone(), true, state.funded, state.pending.is_some());
            let budget = trade::budget_of(&state, live);
            (b.budget, b.stop_at) = (budget, Some(budget * (1.0 - live.stop_total_loss)));
            b.wish = wish(&wish_path(&self.data_dir, id)).filter(|_| state.ended.is_none());
            b.worth = b.worth.filter(|_| state.funded);
            // its worth bar by bar, the newest stretch of it
            let mut equity: Vec<(i64, f64)> =
                store.lab_equity(id, &e.name)?.into_iter().map(|(ts, _, worth, _)| (ts + plan.bar_ms, worth)).collect();
            let keep = equity.len().saturating_sub(BARS_SHOWN);
            equity.drain(..keep);
            b.equity = equity;
            b.journal = store.lab_journal(id)?;
            let keep = b.journal.len().saturating_sub(400);
            b.journal.drain(..keep);
            b.file = match &holder {
                Some(h) if by_name => Some(h.file.clone()),
                _ => file_of(&self.data_dir, &self.rules_dir, id).map(|p| p.display().to_string()),
            };
            bots.push(b);
        }
        // a rules file for real money that has not run yet: there to be started
        for file in rules_files(&self.rules_dir) {
            let Some(plan) = std::fs::read_to_string(&file).ok().and_then(|t| parse(&t).ok()) else { continue };
            let (Some(live), [e]) = (&plan.live, plan.experiments.as_slice()) else { continue };
            let id = format!("trade-{}", plan.id);
            if runs.iter().any(|r| r.0 == id) {
                continue;
            }
            let bars = bars_for(&plan, e.rule.warmup().max(BARS_SHOWN))?;
            let mut b = bot(&plan, e, &Account::default(), &bars, self.zh);
            b.wish = wish(&wish_path(&self.data_dir, &id));
            (b.id, b.real, b.funded, b.state) = (id, true, false, BotState::Stopped);
            (b.budget, b.stop_at) = (live.budget_usd, Some(live.budget_usd * (1.0 - live.stop_total_loss)));
            (b.worth, b.file) = (None, Some(file.display().to_string()));
            bots.push(b);
        }
        // the one that runs first, then those that can be started, then the ended ones
        bots.sort_by_key(|b| match b.state {
            BotState::Running => 0,
            BotState::Stopped => 1,
            _ => 2,
        });
        // the paper run that is newest, each of its rules a line
        if let Some(store) = store
            && let Some((id, _, manifest)) = runs.iter().find(|r| !r.0.starts_with("trade-"))
            && let Ok(plan) = parse(manifest)
        {
            let window = plan.experiments.iter().map(|e| e.rule.warmup()).max().unwrap_or(0).max(BARS_SHOWN);
            let bars = bars_for(&plan, window)?;
            for e in &plan.experiments {
                let acct: Account = match store.lab_state(id, &e.name)? {
                    Some((_, json)) => serde_json::from_str(&json).with_context(|| format!("the state of {id}"))?,
                    None => Account::new(plan.capital),
                };
                let mut b = bot(&plan, e, &acct, &bars, self.zh);
                b.id = format!("{id}/{}", e.name);
                bots.push(b);
            }
        }
        Ok(BotsView { bots, error: None })
    }

    fn act(&self, id: &str, action: BotAction) -> Result<String> {
        let (is_running, holder) = running(&self.data_dir);
        let this_runs = is_running && holder.as_ref().is_none_or(|h| h.run == id);
        let zh = self.zh;
        let say = |en: &'static str, cn: &'static str| if zh { cn } else { en };
        match action {
            BotAction::Stop => {
                if !this_runs {
                    bail!(say("it is not running", "它没有在运行"));
                }
                let Some(h) = holder else {
                    bail!(say(
                        "it was started by an older version that does not say which program it is: stop it in its own window (Ctrl-C)",
                        "它是旧版本启动的，这里不知道是哪个进程：请在它自己的窗口里按 Ctrl-C 停止"
                    ));
                };
                signal(h.pid)?;
                Ok(say(
                    "stopping: it finishes what it is doing, then holds what it has",
                    "正在停止：它会做完手上的事，然后保持现有持仓",
                )
                .into())
            }
            BotAction::Budget { cents } => {
                let to = f64::from(cents) / 100.0;
                if !(trade::MIN_BUDGET_USD..=trade::MAX_BUDGET_USD).contains(&to) {
                    bail!(if zh {
                        format!("预算要在 {:.0} 到 {:.0} 美元之间", trade::MIN_BUDGET_USD, trade::MAX_BUDGET_USD)
                    } else {
                        format!("a budget is between {:.0} and {:.0} USD", trade::MIN_BUDGET_USD, trade::MAX_BUDGET_USD)
                    });
                }
                if !id.starts_with("trade-") {
                    bail!(say("a paper run has no budget to change", "纸面实验没有可以调整的预算"));
                }
                if this_runs && !holder.as_ref().is_some_and(|h| h.wishes) {
                    bail!(say(
                        "it was started by a version that cannot change its budget while it runs: stop it (x) and start it again (s), then change it",
                        "它是旧版本启动的，运行中还不会调整预算：先停止（x）再启动（s），然后再调"
                    ));
                }
                std::fs::create_dir_all(&self.data_dir)?;
                std::fs::write(wish_path(&self.data_dir, id), format!("{to:.2}\n"))?;
                Ok(if this_runs {
                    say(
                        "noted: it changes its budget within seconds, or as soon as it can (its record says which)",
                        "已登记：几秒内生效；现在做不到的话，它会在能做到时再调（以它的记录为准）",
                    )
                } else {
                    say("noted: it changes its budget when it is started", "已登记：它下次启动时生效")
                }
                .into())
            }
            BotAction::Start | BotAction::Close => {
                if is_running {
                    bail!(say(
                        "a bot is running: stop it first (one at a time)",
                        "已有一个机器人在运行：先停止它（同一时间只能跑一个）"
                    ));
                }
                let file = file_of(&self.data_dir, &self.rules_dir, id)
                    .context(say("its rules file was not found", "找不到它的规则文件"))?;
                let plan = super::load(&file)?;
                let live = plan.live.as_ref().context("the file has no [live] section")?;
                live.consent().map_err(anyhow::Error::msg)?;
                if !self.live_enabled {
                    bail!(say(
                        "execution.live_enabled is not true in your config",
                        "你的配置里 execution.live_enabled 不是 true"
                    ));
                }
                let close = action == BotAction::Close;
                self.spawn(id, &file, close)?;
                Ok(if close {
                    say(
                        "closing: what it holds is being sold, then the run ends",
                        "正在平仓：卖出它的持仓，然后这一轮结束",
                    )
                } else {
                    say(
                        "started: it acts after ten seconds, then at each bar's close",
                        "已启动：十秒后开始，之后每根线收盘时判断一次",
                    )
                }
                .into())
            }
        }
    }

    /// The rules file of a new bot, written into the config directory beside
    /// the others. Nothing is started: the page lists it, to be started there.
    /// The words that say it may lose money are the ones the operator typed.
    fn create(&self, spec: &NewBot) -> Result<String> {
        let zh = self.zh;
        let named = !spec.name.is_empty()
            && spec.name.len() <= 20
            && spec.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !named {
            bail!(if zh {
                "名字只能用小写字母、数字和 -，最多 20 个字符"
            } else {
                "a name is small letters, digits and -, twenty at most"
            });
        }
        let text = rules_text(spec);
        let plan = parse(&text).context(if zh {
            "这组数字写不成规则"
        } else {
            "these numbers do not make a rule"
        })?;
        let live = plan.live.as_ref().context("no [live] section")?;
        live.consent().map_err(|_| {
            anyhow::anyhow!(if zh {
                format!("确认词要原样输入 {}：没有它，程序不会发出任何交易", trade::ACK)
            } else {
                format!("the words are {}, as they are: without them nothing is ever sent", trade::ACK)
            })
        })?;
        // the same rule with the same budget as one there is already would be that one, not a new one
        let id = format!("trade-{}", plan.id);
        if self.read()?.bots.iter().any(|b| b.id == id) {
            bail!(if zh {
                "已经有一个完全一样的机器人了：改一个数字再建"
            } else {
                "there is one just like it already: change a number"
            });
        }
        let path = self.rules_dir.join(format!("trade-{}.toml", spec.name));
        std::fs::create_dir_all(&self.rules_dir)?;
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(if zh {
                format!("已经有 {} 这个文件了：换个名字", path.display())
            } else {
                format!("{} is there already: give it another name", path.display())
            }),
            Err(e) => return Err(e).with_context(|| format!("writing {}", path.display())),
        };
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        Ok(if zh {
            format!("已创建 {}（{}）。它还没启动：按 s 启动", spec.name, path.display())
        } else {
            format!("{} is made ({}). It is not started: s starts it", spec.name, path.display())
        })
    }

    /// `--trade FILE` as a program of its own: no terminal, its own process
    /// group (a Ctrl-C here is not one there), what it prints kept in a file.
    fn spawn(&self, run: &str, file: &Path, close: bool) -> Result<()> {
        let exe = std::env::current_exe().context("this program's own path")?;
        let log_path = self.data_dir.join(format!("{run}.log"));
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        let mut cmd = std::process::Command::new(exe);
        if let Some(c) = &self.config_file {
            cmd.arg("--config").arg(c);
        }
        cmd.arg("--trade").arg(file);
        if close {
            cmd.arg("--close");
        }
        cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().context("starting it")?;
        // waited for, so that it does not linger as a dead entry once it ends
        std::thread::spawn(move || drop(child.wait()));
        Ok(())
    }
}

/// The Ctrl-C a program gets in its own window.
fn signal(pid: u32) -> Result<()> {
    if !cfg!(unix) {
        bail!("stop it in its own window (Ctrl-C)");
    }
    let done = std::process::Command::new("kill").args(["-INT", &pid.to_string()]).status().context("kill")?;
    if !done.success() {
        bail!("it could not be signalled");
    }
    Ok(())
}

/// A new bot's rules file, as it would be written by hand.
fn rules_text(spec: &NewBot) -> String {
    format!(
        "# Made on the Bots page. A rule that buys SOL when a 15-minute bar closes k deviations\n\
         # under the average of its window, and sells when it is back over it. See docs/TRADE.md.\n\
         instrument = \"SOL-USDT\"\n\
         bar = \"15m\"\n\n\
         [live]\n\
         budget_usd = {budget:?}\n\
         stop_total_loss = {total:?}\n\
         acknowledge = \"{ack}\"\n\n\
         [[experiment]]\n\
         name = \"{name}\"\n\
         rule = \"dip\"\n\
         window = {window}\n\
         k = {k:?}\n\
         exit_z = 0.0\n\
         stop = {stop:?}\n",
        budget = spec.budget,
        total = spec.total_stop,
        ack = spec.acknowledge.replace(['"', '\\', '\n'], ""),
        name = spec.name,
        window = spec.window,
        k = spec.k,
        stop = spec.stop,
    )
}

/// The TUI's way to the bots.
pub fn port(cfg: &Config, config_file: Option<PathBuf>, zh: bool) -> BotPort {
    let desk = Arc::new(Desk {
        data_dir: cfg.data_dir(),
        rules_dir: searcher_core::config::user_dir(),
        config_file,
        zh,
        live_enabled: cfg.execution.live_enabled,
        store: Mutex::new(None),
    });
    let (reader, maker) = (desk.clone(), desk.clone());
    BotPort {
        view: Arc::new(move || {
            reader.read().unwrap_or_else(|e| BotsView {
                error: Some(format!("the bots could not be read: {e:#}")),
                ..Default::default()
            })
        }),
        act: Arc::new(move |id, action| desk.act(id, action).map_err(|e| format!("{e:#}"))),
        create: Arc::new(move |spec| maker.create(spec).map_err(|e| format!("{e:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Duration;

    /// A key of a `KEYS` script: a character, or `⏎` for Enter.
    fn ratatui_key(c: char) -> KeyEvent {
        KeyEvent::new(if c == '⏎' { KeyCode::Enter } else { KeyCode::Char(c) }, KeyModifiers::NONE)
    }

    const FILE: &str = r#"
bar = "15m"

[live]
budget_usd = 2.0
stop_total_loss = 0.5
acknowledge = "ALLOW LOSS"

[[experiment]]
name = "dip-1d"
rule = "dip"
window = 4
k = 1.0
stop = 0.05
"#;

    fn desk(dir: &Path) -> Desk {
        Desk {
            data_dir: dir.to_path_buf(),
            rules_dir: dir.join("rules"),
            config_file: None,
            zh: false,
            live_enabled: true,
            store: Mutex::new(None),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mobius-desk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_real_run_is_shown_as_its_records_say() {
        let dir = scratch("view");
        assert_eq!(desk(&dir).read().unwrap().bots, Vec::new(), "no records: no bots, and no file made");
        assert!(!dir.join("research.sqlite").exists());
        // a rules file for real money that has not run yet is a bot that can be started
        std::fs::create_dir_all(dir.join("rules")).unwrap();
        std::fs::write(dir.join("rules").join("mine.toml"), FILE).unwrap();
        std::fs::write(dir.join("rules").join("config.toml"), "[general]\nmode = \"paper\"\n").unwrap();
        let listed = desk(&dir).read().unwrap().bots;
        assert_eq!(listed.len(), 1, "the rules file, not the config beside it");
        assert_eq!(
            (listed[0].name.as_str(), &listed[0].state, listed[0].funded),
            ("dip-1d", &BotState::Stopped, false)
        );
        assert!(
            listed[0].can(BotAction::Start, false).is_ok()
                && listed[0].file.as_deref().is_some_and(|f| f.ends_with("mine.toml"))
        );
        std::fs::remove_file(dir.join("rules").join("mine.toml")).unwrap();

        let plan = parse(FILE).unwrap();
        let run = format!("trade-{}", plan.id);
        let store = ResearchStore::open(&dir.join("research.sqlite")).unwrap();
        store.begin_lab_run(&run, 1, "test", &plan.manifest).unwrap();
        // five bars ending now; it bought at the fourth
        let now = searcher_core::Ts::now().millis() / plan.bar_ms * plan.bar_ms;
        let closes = [100.0, 101.0, 99.0, 100.0, 98.0];
        let rows: Vec<_> =
            closes.iter().enumerate().map(|(i, c)| (now - (5 - i as i64) * plan.bar_ms, *c, *c, *c, *c, 1.0)).collect();
        store.insert_lab_bars(&plan.instrument, &plan.bar, &rows).unwrap();
        let mut state = trade::State { funded: true, ..Default::default() };
        state.account.cash = 2.0;
        state.account.bought(2.0, 0.02, rows[3].0 + 1, 100.0);
        store.set_lab_state(&run, "dip-1d", rows[4].0, &serde_json::to_string(&state).unwrap()).unwrap();
        store.insert_lab_journal(&run, now, "bought 0.020000 SOL for 2.0000 USDC").unwrap();
        drop(store);
        let rules = dir.join("rules.toml");
        std::fs::write(&rules, FILE).unwrap();
        std::fs::write(dir.join(format!("{run}.path")), rules.display().to_string()).unwrap();

        let view = desk(&dir).read().unwrap();
        assert_eq!(view.bots.len(), 1);
        let b = &view.bots[0];
        assert_eq!(
            (b.id.as_str(), b.name.as_str(), b.real, &b.state),
            (run.as_str(), "dip-1d", true, &BotState::Stopped)
        );
        assert_eq!((b.budget, b.stop_at, b.cash, b.sol, b.paid), (2.0, Some(1.0), 0.0, 0.02, 2.0));
        assert_eq!((b.inst.as_str(), b.bar.as_str(), b.bar_ms), ("SOL-USDT", "15m", 900_000));
        // the numbers its prices come from: the average and deviation of its four bars
        let c = b.calc.as_ref().expect("a dip rule shows its arithmetic");
        assert_eq!((c.window, c.k, c.stop), (4, 1.0, Some(0.05)));
        assert!((c.mean - 99.5).abs() < 1e-9 && (b.levels.buy.unwrap() - (c.mean - c.sd)).abs() < 1e-9, "{c:?}");
        assert!((b.worth.unwrap() - 0.02 * 98.0).abs() < 1e-9, "what it holds at the last close: {:?}", b.worth);
        assert_eq!(b.levels.stop, Some(95.0), "5 % under the 100 it paid");
        assert!(b.levels.buy.unwrap() < b.levels.sell.unwrap());
        assert_eq!(b.closes.len(), 5);
        assert_eq!(b.fills, vec![(rows[3].0 + 1, true)]);
        assert_eq!(b.journal.len(), 1);
        assert_eq!(b.file.as_deref(), Some(rules.display().to_string().as_str()), "the file it starts from");
        assert!(
            b.rule.starts_with("Buys when a bar closes 1 deviation under its average of 4 bars (1 h)"),
            "{}",
            b.rule
        );
        // a budget changed by hand is the run's own, and what it is sold out at follows it
        let store = ResearchStore::open(&dir.join("research.sqlite")).unwrap();
        state.budget = Some(4.0);
        store.set_lab_state(&run, "dip-1d", rows[4].0, &serde_json::to_string(&state).unwrap()).unwrap();
        drop(store);
        let b = desk(&dir).read().unwrap().bots.remove(0);
        assert_eq!((b.budget, b.stop_at, b.wish), (4.0, Some(2.0), None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Bots page as it would be drawn from this machine's own records
    /// (read only): `cargo test -p mobius-searcher --lib this_machine -- --ignored --nocapture`,
    /// `SIZE=120x40` for another size.
    #[test]
    #[ignore]
    fn the_bots_page_of_this_machine() {
        let size = std::env::var("SIZE").unwrap_or_else(|_| "200x58".into());
        let zh = std::env::var("ZH").is_ok();
        let (w, h) = size.split_once('x').unwrap();
        let opts = searcher_tui::TuiOptions {
            bots: Some(port(&Config::default(), None, zh)),
            zh,
            ..searcher_tui::TuiOptions::default()
        };
        let mut app = searcher_tui::App::new(&opts);
        app.glyphs = searcher_tui::theme::Glyphs::unicode();
        app.theme = searcher_tui::theme::Theme::with_depth(searcher_tui::theme::Depth::TrueColor);
        // WALLET=<address>: with that wallet's balances, read from the chain (the budget form shows what is free)
        if let Ok(address) = std::env::var("WALLET") {
            let mut cfg = Config::default();
            cfg.wallet.pubkey = Some(address);
            let port = crate::purse::port(&cfg, zh);
            std::thread::sleep(Duration::from_secs(8));
            app.wallet = Some(searcher_tui::wallet::Wallet::fixed((port.view)()));
        }
        let vm = searcher_tui::ViewModel::new(true);
        let key = |c| ratatui_key(c);
        // LIVE: with the exchange's candles, as the running program has them
        if std::env::var("LIVE").is_ok() {
            app.cex = Some(searcher_tui::cex::Cex::start(searcher_tui::cex::OkxSource::default(), 0, 1));
        }
        app.on_key(key('9'), &vm);
        for c in std::env::var("KEYS").unwrap_or_default().chars() {
            app.on_key(key(c), &vm);
        }
        if app.cex.is_some() {
            std::thread::sleep(Duration::from_secs(5));
        }
        let buf = searcher_tui::snapshot(&mut app, &vm, w.parse().unwrap(), h.parse().unwrap());
        if let Ok(to) = std::env::var("HTML") {
            std::fs::write(to, searcher_tui::buffer_html(&buf, "bots")).unwrap();
        }
        println!("{}", searcher_tui::buffer_text(&buf));
    }

    #[test]
    fn a_new_bot_is_a_rules_file_the_operator_gave_their_word_for() {
        let dir = scratch("new");
        let d = desk(&dir);
        let spec = NewBot {
            name: "dip-test".into(),
            window: 96,
            k: 1.5,
            stop: 0.04,
            budget: 3.0,
            total_stop: 0.4,
            acknowledge: "ALLOW LOSS".into(),
        };
        // without the words nothing is written; nor with a name that is no file's
        let err = |s: &NewBot| format!("{:#}", d.create(s).unwrap_err());
        assert!(err(&NewBot { acknowledge: "allow loss".into(), ..spec.clone() }).contains("ALLOW LOSS"));
        assert!(err(&NewBot { name: "../x".into(), ..spec.clone() }).contains("small letters"));
        assert!(err(&NewBot { budget: 26.0, ..spec.clone() }).contains("do not make a rule"));
        assert!(!dir.join("rules").exists() || std::fs::read_dir(dir.join("rules")).unwrap().next().is_none());
        // made: a file like one written by hand, listed to be started, and nothing running
        let said = d.create(&spec).unwrap();
        assert!(said.contains("It is not started"), "{said}");
        let text = std::fs::read_to_string(dir.join("rules/trade-dip-test.toml")).unwrap();
        let plan = parse(&text).unwrap();
        let live = plan.live.clone().unwrap();
        assert_eq!((live.budget_usd, live.stop_total_loss, live.consent()), (3.0, 0.4, Ok(())));
        assert!(text.contains("rule = \"dip\"") && text.contains("window = 96") && text.contains("k = 1.5"), "{text}");
        let view = d.read().unwrap();
        let b = &view.bots[0];
        assert_eq!((b.name.as_str(), b.real, &b.state, b.budget), ("dip-test", true, &BotState::Stopped, 3.0));
        assert!((b.stop_at.unwrap() - 1.8).abs() < 1e-9, "sold out at 60 % of its budget");
        assert!(!running(&dir).0);
        // the same again is that one, not a new one; and its file is never written over
        assert!(err(&spec).contains("just like it"));
        assert!(err(&NewBot { k: 2.0, ..spec.clone() }).contains("is there already"), "its file is not written over");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_lock_is_a_running_bot_and_actions_are_refused_as_they_should() {
        let dir = scratch("lock");
        let plan = parse(FILE).unwrap();
        let run = format!("trade-{}", plan.id);
        let store = ResearchStore::open(&dir.join("research.sqlite")).unwrap();
        store.begin_lab_run(&run, 1, "test", &plan.manifest).unwrap();
        drop(store);
        let d = desk(&dir);
        assert_eq!(d.read().unwrap().bots[0].state, BotState::Stopped);
        let err = |a| format!("{:#}", d.act(&run, a).unwrap_err());
        assert!(err(BotAction::Stop).contains("not running"));
        assert!(err(BotAction::Start).contains("rules file was not found"), "{}", err(BotAction::Start));
        // its budget: a wish is left for its program, which takes it when it starts
        assert!(d.act(&run, BotAction::Budget { cents: 500 }).unwrap().contains("when it is started"));
        assert_eq!(std::fs::read_to_string(wish_path(&dir, &run)).unwrap().trim(), "5.00");
        assert_eq!(d.read().unwrap().bots[0].wish, Some(5.0));
        assert!(err(BotAction::Budget { cents: 3000 }).contains("between 1 and 25"));
        assert!(format!("{:#}", d.act("lab-1/dip", BotAction::Budget { cents: 500 }).unwrap_err()).contains("paper"));

        // the lock held, as `--trade` holds it, and what it says of itself
        let lock = std::fs::File::create(dir.join("trade.lock")).unwrap();
        lock.try_lock().unwrap();
        assert_eq!(d.read().unwrap().bots[0].state, BotState::Running, "held by a version that did not say which run");
        assert!(err(BotAction::Stop).contains("older version"));
        assert!(err(BotAction::Budget { cents: 400 }).contains("cannot change its budget while it runs"));
        assert_eq!(wish(&wish_path(&dir, &run)), Some(5.0), "a wish that was refused replaces none");
        let rules = dir.join("rules.toml");
        std::fs::write(&rules, FILE).unwrap();
        hold(&dir, &run, &rules).unwrap();
        let b = d.read().unwrap().bots.remove(0);
        assert_eq!(b.state, BotState::Running);
        assert!(b.file.is_some_and(|f| f.ends_with("rules.toml")));
        // a program that says it reads wishes is left one
        assert!(d.act(&run, BotAction::Budget { cents: 400 }).unwrap().contains("within seconds"));
        assert_eq!(d.read().unwrap().bots[0].wish, Some(4.0));
        assert!(err(BotAction::Start).contains("one at a time"));
        assert!(err(BotAction::Close).contains("one at a time"));

        drop(lock);
        // (a test beside this one may be starting a program just now, which holds a copy of the lock for an instant)
        let mut state = BotState::Running;
        for _ in 0..50 {
            state = d.read().unwrap().bots[0].state.clone();
            if state == BotState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(state, BotState::Stopped, "the lock let go: stopped, whatever the note says");
        // without the words, or with the switch off, nothing is started
        std::fs::write(&rules, FILE.replace("ALLOW LOSS", "")).unwrap();
        assert!(err(BotAction::Start).contains("rules file was not found"), "a changed file is another run");
        std::fs::write(&rules, FILE).unwrap();
        let off = Desk { live_enabled: false, ..desk(&dir) };
        assert!(format!("{:#}", off.act(&run, BotAction::Start).unwrap_err()).contains("live_enabled"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
