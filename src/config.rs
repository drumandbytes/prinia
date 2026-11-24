use std::fs;
use std::path::{Path, PathBuf};
use toml::{Table, Value};

#[derive(Clone, Copy)]
pub enum Ty {
    Bool,
    Str,
    Num,
}

/// Every flat setting. Each one also reads the env var `PRINIA_<KEY>`.
/// Precedence: flag > env > nearest `.priniarc` > parent `.priniarc` files > global config > default.
/// `[providers.<name>]` tables are the one nested setting.
pub const KEYS: &[(&str, Ty)] = &[
    ("provider", Ty::Str),
    ("model", Ty::Str),
    ("fast_model", Ty::Str),
    ("deep_model", Ty::Str),
    ("effort", Ty::Str),
    ("fast_effort", Ty::Str),
    ("deep_effort", Ty::Str),
    ("max_tokens", Ty::Num),
    ("record", Ty::Bool),
    ("cache", Ty::Str),
    ("cache_ttl", Ty::Str),
    ("context", Ty::Num),
    ("context_chars", Ty::Num),
    ("max_size", Ty::Str),
    ("max_age", Ty::Str),
    ("fast_under", Ty::Num),
    ("deep_over", Ty::Num),
    ("history_scan", Ty::Num),
    ("excerpt_prompt", Ty::Num),
    ("excerpt_reply", Ty::Num),
    ("agent", Ty::Bool),
    ("write", Ty::Bool),
    ("run", Ty::Bool),
    ("agent_steps", Ty::Num),
    ("check_retries", Ty::Num),
    ("keychain", Ty::Bool),
    ("prompt_pipe", Ty::Str),
    ("prompt_interactive", Ty::Str),
];

/// Keys a repository's `.priniarc` may not set until the user runs `prinia trust`: they redirect
/// the API key or let the model act on the machine.
const RISKY: &[&str] = &["providers", "run", "write"];

fn trust_path() -> Option<PathBuf> {
    Some(global_path()?.with_file_name("trusted"))
}

fn digest(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn is_trusted(text: &str) -> bool {
    trust_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .is_some_and(|t| t.lines().any(|l| l.trim() == digest(text)))
}

/// Trusts the exact current contents of every `.priniarc` above `cwd`; an edit revokes it.
pub fn trust(cwd: &Path) -> std::io::Result<Vec<PathBuf>> {
    let Some(tp) = trust_path() else {
        return Ok(Vec::new());
    };
    let mut known = fs::read_to_string(&tp).unwrap_or_default();
    let mut done = Vec::new();
    for p in cwd.ancestors().map(|d| d.join(".priniarc")) {
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        if !is_trusted(&text) {
            known.push_str(&digest(&text));
            known.push('\n');
        }
        done.push(p);
    }
    if let Some(d) = tp.parent() {
        fs::create_dir_all(d)?;
    }
    fs::write(tp, known)?;
    Ok(done)
}

#[derive(Clone, Copy, PartialEq)]
enum Layer {
    Project,
    Env,
    Global,
}

pub struct Config {
    /// `.priniarc` files, nearest last.
    project: Table,
    global: Table,
    providers: Table,
    pub files: Vec<PathBuf>,
}

pub fn global_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PRINIA_CONFIG") {
        return Some(p.into());
    }
    #[cfg(windows)]
    return Some(
        directories::ProjectDirs::from("", "", "prinia")?
            .config_dir()
            .join("config.toml"),
    );
    #[cfg(not(windows))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join(".config")))?;
        Some(base.join("prinia/config.toml"))
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(format!("PRINIA_{}", key.to_uppercase()))
        .ok()
        .filter(|v| !v.is_empty())
}

fn fits(ty: Ty, v: &Value) -> bool {
    match ty {
        Ty::Bool => v.is_bool(),
        Ty::Str => v.is_str(),
        Ty::Num => v.as_integer().is_some_and(|n| n >= 0),
    }
}

impl Config {
    pub fn load(cwd: &Path) -> Config {
        Self::load_from(global_path(), cwd)
    }

    pub fn load_from(global: Option<PathBuf>, cwd: &Path) -> Config {
        let mut paths: Vec<PathBuf> = global.iter().cloned().collect();
        let mut dirs: Vec<PathBuf> = cwd.ancestors().map(|d| d.join(".priniarc")).collect();
        dirs.reverse(); // farthest first, so the nearest file wins
        paths.extend(dirs);

        let mut cfg = Config {
            project: Table::new(),
            global: Table::new(),
            providers: Table::new(),
            files: Vec::new(),
        };
        for p in paths {
            let Ok(text) = fs::read_to_string(&p) else {
                continue;
            };
            let global = Some(&p) == global.as_ref();
            let trusted = global || is_trusted(&text);
            let t = match toml::from_str::<Table>(&text) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[prinia] ignoring {}: {e}", p.display());
                    continue;
                }
            };
            for (k, v) in t {
                // a repo may always tighten (`run = false`); loosening needs trust
                if !trusted && RISKY.contains(&k.as_str()) && v != Value::Boolean(false) {
                    eprintln!(
                        "[prinia] {}: `{k}` ignored until you run `prinia trust` here",
                        p.display()
                    );
                    continue;
                }
                // a typo must not silently change behaviour, and a bad value must not shadow a
                // good one from a lower layer
                if k == "providers" {
                    match v {
                        Value::Table(ps) => cfg
                            .providers
                            .extend(ps.into_iter().filter(|(_, p)| p.is_table())),
                        _ => eprintln!(
                            "[prinia] {}: `providers` must be a table; ignored",
                            p.display()
                        ),
                    }
                    continue;
                }
                match KEYS.iter().find(|(n, _)| *n == k) {
                    None => eprintln!(
                        "[prinia] {}: unknown key `{k}` (see `prinia config`)",
                        p.display()
                    ),
                    Some((_, ty)) if !fits(*ty, &v) => eprintln!(
                        "[prinia] {}: key `{k}` has the wrong type; ignored",
                        p.display()
                    ),
                    Some(_) => {
                        if global {
                            cfg.global.insert(k, v);
                        } else {
                            cfg.project.insert(k, v);
                        }
                    }
                }
            }
            cfg.files.push(p);
        }
        cfg
    }

    /// Directory settings win over the environment, which wins over the global file, so a
    /// project can say "never record" and have it hold. Flags are applied by the caller.
    fn layered(&self, key: &str) -> Option<(Layer, Value)> {
        if let Some(v) = self.project.get(key) {
            return Some((Layer::Project, v.clone()));
        }
        if let Some(e) = env(key) {
            return Some((Layer::Env, Value::String(e)));
        }
        self.global.get(key).map(|v| (Layer::Global, v.clone()))
    }

    pub fn str(&self, key: &str) -> Option<String> {
        self.layered(key)?.1.as_str().map(str::to_owned)
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.layered(key)? {
            (Layer::Env, Value::String(e)) => match e.to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            },
            (_, v) => v.as_bool(),
        }
    }

    pub fn num(&self, key: &str) -> Option<u64> {
        match self.layered(key)? {
            (Layer::Env, Value::String(e)) => e.parse().ok(),
            (_, v) => v.as_integer().map(|n| n as u64),
        }
    }

    /// A field of `[providers.<name>]`.
    pub fn provider_str(&self, name: &str, field: &str) -> Option<String> {
        self.providers
            .get(name)?
            .get(field)?
            .as_str()
            .map(str::to_owned)
    }

    pub fn provider_names(&self) -> Vec<&String> {
        self.providers.keys().collect()
    }

    /// Effective value and where it came from, for `prinia config`.
    pub fn describe(&self, key: &str) -> (String, &'static str) {
        match self.layered(key) {
            Some((Layer::Project, v)) => (v.to_string(), "directory"),
            Some((Layer::Env, Value::String(e))) => (e, "env"),
            Some((_, v)) => (v.to_string(), "global"),
            None => ("-".into(), "default"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layering() {
        let root = std::env::temp_dir().join(format!("prinia-cfg-{}", std::process::id()));
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        let global = root.join("global.toml");
        fs::write(&global, "record = false\nmodel = \"g\"\nmax_tokens = 100\n[providers.local]\ntype = \"openai\"\nbase_url = \"http://x/v1\"\n").unwrap();
        fs::write(root.join(".priniarc"), "model = \"proj\"\nrecrod = true\n").unwrap();
        fs::write(sub.join(".priniarc"), "record = true\nmodel = 5\n").unwrap();

        let c = Config::load_from(Some(global), &sub);
        assert_eq!(c.bool("record"), Some(true), "nearest file wins");
        assert_eq!(
            c.str("model").as_deref(),
            Some("proj"),
            "a wrongly typed value does not shadow the parent's"
        );
        assert_eq!(
            c.num("max_tokens"),
            Some(100),
            "global applies when nothing nearer sets it"
        );
        assert_eq!(
            c.provider_str("local", "base_url").as_deref(),
            Some("http://x/v1")
        );
        assert_eq!(c.provider_str("nope", "type"), None);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn directory_beats_env_beats_global() {
        let root = std::env::temp_dir().join(format!("prinia-order-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let global = root.join("g.toml");
        fs::write(&global, "fast_under = 1\ncache_ttl = \"1d\"\n").unwrap();
        fs::write(root.join(".priniarc"), "fast_under = 2\n").unwrap();
        // SAFETY: only this test touches these variables
        unsafe {
            std::env::set_var("PRINIA_FAST_UNDER", "3");
            std::env::set_var("PRINIA_CACHE_TTL", "2d");
        }
        let c = Config::load_from(Some(global), &root);
        assert_eq!(c.num("fast_under"), Some(2), "directory beats env");
        assert_eq!(
            c.str("cache_ttl").as_deref(),
            Some("2d"),
            "env beats global"
        );
        unsafe {
            std::env::remove_var("PRINIA_FAST_UNDER");
            std::env::remove_var("PRINIA_CACHE_TTL");
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
