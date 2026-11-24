//! `prinia update`: runs the release's own installer into the folder this binary lives in, so
//! the download and checksum code exists once (scripts/install.sh). `--check` only compares versions.
use anyhow::{Context, Result, anyhow, bail};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

const REPO: &str = "drumandbytes/prinia";

/// Paths of binaries that a package manager or a build tree owns: updating them here would fight it.
fn managed_by(path: &str) -> Option<&'static str> {
    if path.contains("/Cellar/") || path.contains("/homebrew/") || path.contains("/linuxbrew/") {
        Some("installed by Homebrew: run `brew upgrade prinia`")
    } else if path.contains("/.cargo/") {
        Some(
            "installed by cargo: run `cargo install --locked --force prinia` (or from your checkout)",
        )
    } else if path.contains("/target/debug/") || path.contains("/target/release/") {
        Some("this is a development build; rebuild it with cargo")
    } else {
        None
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

/// The newest release tag, e.g. `v0.2.0`. `PRINIA_RELEASES_API` replaces the GitHub API URL (tests, mirrors).
fn latest_tag() -> Result<String> {
    let url = std::env::var("PRINIA_RELEASES_API")
        .unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"));
    let v: serde_json::Value = agent()
        .get(&url)
        .header("user-agent", "prinia")
        .call()
        .with_context(|| format!("cannot reach {url}"))?
        .body_mut()
        .read_json()?;
    v["tag_name"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("no release found at {url}"))
}

fn parts(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split(['.', '-', '+'])
        .map_while(|p| p.parse().ok())
        .collect()
}

pub fn run(check: bool) -> Result<()> {
    if cfg!(windows) {
        bail!("update is not supported on Windows yet; build from source");
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let current = env!("CARGO_PKG_VERSION");
    let latest = latest_tag()?;
    use std::cmp::Ordering::*;
    match parts(&latest).cmp(&parts(current)) {
        Equal => {
            println!("prinia {current} is the latest release");
            return Ok(());
        }
        Less => {
            println!("prinia {current} is newer than the latest release ({latest})");
            return Ok(());
        }
        Greater => {}
    }
    if check {
        println!("prinia {current} -> {latest} is available: run `prinia update`");
        return Ok(());
    }
    if let Some(why) = managed_by(&exe.to_string_lossy()) {
        bail!("{} is {why}", exe.display());
    }
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("no install directory"))?;
    let base = std::env::var("PRINIA_DOWNLOAD_URL")
        .unwrap_or_else(|_| format!("https://github.com/{REPO}/releases/latest/download"));
    let script = agent()
        .get(&format!("{base}/install.sh"))
        .header("user-agent", "prinia")
        .call()
        .context("cannot download the installer")?
        .body_mut()
        .read_to_string()?;
    println!("Updating {} from {current} to {latest}", exe.display());
    let mut child = Command::new("sh")
        .arg("-s")
        .env("PRINIA_INSTALL_DIR", dir)
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(script.as_bytes())?;
    if !child.wait()?.success() {
        bail!("the installer failed; prinia was not changed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number_not_text() {
        assert!(parts("v0.10.0") > parts("0.9.9"));
        assert_eq!(parts("v1.2.3"), parts("1.2.3"));
        assert!(parts("v1.2.3-rc1") >= parts("1.2.3"));
    }

    #[test]
    fn package_managed_and_dev_builds_are_not_self_updated() {
        assert!(managed_by("/opt/homebrew/Cellar/prinia/0.1.0/bin/prinia").is_some());
        assert!(managed_by("/home/u/.cargo/bin/prinia").is_some());
        assert!(managed_by("/home/u/prinia/target/debug/prinia").is_some());
        assert!(managed_by("/home/u/.local/bin/prinia").is_none());
    }
}
