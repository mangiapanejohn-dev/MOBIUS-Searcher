//! One process at a time spends the Jupiter budget. Jupiter counts requests
//! per organisation, so two processes with the same key share one bucket and
//! would starve each other. `--research` and every trading session hold this
//! lock while they run; the second one to start is refused.

use anyhow::{Result, bail};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::Path;

pub struct BudgetLock {
    _file: File,
}

/// Take the lock in `data_dir`, recording `holder` (e.g. `--research`) for
/// whoever is refused next.
pub fn acquire(data_dir: &Path, holder: &str) -> Result<BudgetLock> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("jupiter-budget.lock");
    let mut file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path)?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0)?;
            write!(
                file,
                "{holder} · pid {} · since {}",
                std::process::id(),
                searcher_core::Ts::now().format("%Y-%m-%d %H:%M:%S")
            )?;
            file.flush()?;
            Ok(BudgetLock { _file: file })
        }
        Err(TryLockError::WouldBlock) => {
            let mut by = String::new();
            let _ = File::open(&path).and_then(|mut f| f.read_to_string(&mut by));
            let by = if by.trim().is_empty() { "another MØBIUS process".to_string() } else { by.trim().to_string() };
            bail!(
                "the Jupiter budget is in use: {by}.\n\
                 Jupiter's rate limit is per organisation, so two processes would share one bucket. \
                 Stop that process first (the lock is {}).",
                path.display()
            )
        }
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Keeps macOS from idle- and system-sleeping while the process runs
/// (`caffeinate -i -s -w <pid>`; `-s` applies on AC power only, so a closed
/// lid on battery still sleeps). A no-op elsewhere.
pub struct KeepAwake(Option<std::process::Child>);

impl KeepAwake {
    pub fn start() -> Self {
        #[cfg(target_os = "macos")]
        {
            let child = std::process::Command::new("caffeinate")
                .args(["-i", "-s", "-w", &std::process::id().to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok();
            KeepAwake(child)
        }
        #[cfg(not(target_os = "macos"))]
        {
            KeepAwake(None)
        }
    }

    pub fn off() -> Self {
        KeepAwake(None)
    }

    pub fn active(&self) -> bool {
        self.0.is_some()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(c) = &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_is_refused_and_told_who_holds_it() {
        let dir = std::env::temp_dir().join(format!("mobius-budget-{}", std::process::id()));
        let first = acquire(&dir, "--research").unwrap();
        let err = acquire(&dir, "PAPER session").err().unwrap().to_string();
        assert!(err.contains("--research"), "{err}");
        drop(first);
        acquire(&dir, "PAPER session").expect("free again after the holder exits");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
