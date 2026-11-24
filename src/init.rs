//! `prinia init`: find a local model server, pick a model that fits this machine, write the config.
use crate::config;
use anyhow::{Result, anyhow};
use std::io::{BufRead, IsTerminal, Write};
use std::process::Command;
use std::time::Duration;

const OLLAMA: &str = "http://localhost:11434";

/// `PRINIA_OLLAMA_URL` points at another server (and lets tests avoid the real one).
fn ollama() -> String {
    std::env::var("PRINIA_OLLAMA_URL").unwrap_or_else(|_| OLLAMA.into())
}

/// (regular model, approximate download in GB, deep model) for this much memory.
fn recommend(gb: u64) -> (&'static str, u64, Option<&'static str>) {
    match gb {
        40.. => ("qwen2.5:14b", 9, Some("qwen2.5:32b")),
        16.. => ("qwen2.5:14b", 9, None),
        8.. => ("qwen2.5:7b", 5, None),
        _ => ("llama3.2:3b", 2, None),
    }
}

fn memory_gb() -> Option<u64> {
    let bytes = if cfg!(target_os = "macos") {
        let out = Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .ok()?
    } else {
        let t = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = t
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        kb * 1024
    };
    Some(bytes / (1 << 30))
}

fn installed() -> Option<Vec<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    let v: serde_json::Value = agent
        .get(format!("{}/api/tags", ollama()))
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()?;
    Some(
        v["models"]
            .as_array()?
            .iter()
            .filter_map(|m| m["name"].as_str().map(str::to_owned))
            .collect(),
    )
}

fn ask(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut l = String::new();
    let _ = std::io::stdin().lock().read_line(&mut l);
    l.trim().eq_ignore_ascii_case("y")
}

fn pull(model: &str, gb: u64) -> Result<()> {
    if installed().is_some_and(|m| m.iter().any(|n| n == model)) {
        return Ok(());
    }
    if ask(&format!(
        "Download {model} (about {gb} GB) with `ollama pull`?"
    )) {
        Command::new("ollama").args(["pull", model]).status()?;
    } else {
        println!("Later: ollama pull {model}");
    }
    Ok(())
}

/// The global config for a local Ollama model.
fn config_text(url: &str, model: &str, deep: Option<&str>) -> String {
    let mut cfg = format!(
        "provider = \"local\"\nmax_input = 48000   # longer input is processed in parts\n\n[providers.local]\ntype = \"openai\"\ntags = [\"private\"]\nbase_url = \"{url}/v1\"\nmodel = \"{model}\"\nfast_model = \"{model}\"\n"
    );
    if let Some(d) = deep {
        cfg.push_str(&format!("deep_model = \"{d}\"\n"));
    }
    cfg
}

pub fn run() -> Result<()> {
    let keys = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()));
    let Some(_) = installed() else {
        if keys {
            println!("An API key is set in your environment: `prinia \"hello\"` already works.");
        } else {
            println!(
                "No model server found at {} and no API key set.\nInstall Ollama (https://ollama.com), start it and run `prinia init` again, or export ANTHROPIC_API_KEY / OPENAI_API_KEY.",
                ollama()
            );
        }
        return Ok(());
    };
    let gb = memory_gb().unwrap_or_else(|| {
        println!("Could not read this machine's memory size; assuming 8 GB (a small model).");
        8
    });
    let (model, size, deep) = recommend(gb);
    println!(
        "Ollama is running; this machine has about {gb} GB of memory. Recommended: {model}{}.",
        deep.map(|d| format!(" (deep: {d})")).unwrap_or_default()
    );
    pull(model, size)?;
    if let Some(d) = deep {
        pull(d, 20)?;
    }
    let cfg = config_text(&ollama(), model, deep);
    let path = config::global_path().ok_or_else(|| anyhow!("no config directory"))?;
    if path.exists() {
        println!(
            "{} already exists and was not changed. Add this to use the model:\n\n{cfg}",
            path.display()
        );
    } else {
        std::fs::create_dir_all(path.parent().ok_or_else(|| anyhow!("bad config path"))?)?;
        std::fs::write(&path, cfg)?;
        println!("Wrote {}. Try: prinia \"hello\"", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_config_is_valid_toml_with_the_model_private_and_tiered() {
        let t: toml::Table = toml::from_str(&config_text("http://h:1", "m1", Some("m2"))).unwrap();
        let p = &t["providers"]["local"];
        assert_eq!(p["base_url"].as_str(), Some("http://h:1/v1"));
        assert_eq!(p["tags"][0].as_str(), Some("private"));
        assert_eq!(
            (p["model"].as_str(), p["deep_model"].as_str()),
            (Some("m1"), Some("m2"))
        );
        assert_eq!(t["max_input"].as_integer(), Some(48000));
    }

    #[test]
    fn model_scales_with_memory() {
        assert_eq!(recommend(48).2, Some("qwen2.5:32b"));
        assert_eq!(recommend(16).0, "qwen2.5:14b");
        assert_eq!(recommend(8).0, "qwen2.5:7b");
        assert_eq!(recommend(4).0, "llama3.2:3b");
    }
}
