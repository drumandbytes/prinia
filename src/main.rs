use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use config::Config;
use provider::{Provider, Request, Tier};
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio, exit};
use std::time::Instant;
use store::{Content, Meta, Store};

mod agent;
mod check;
mod config;
mod context;
mod curate;
mod history;
mod init;
mod key;
mod keychain;
mod provider;
mod stats;
mod store;

const PROMPT_PIPE: &str = "You are a command-line tool in a UNIX pipeline. Output ONLY the raw text or data requested. No markdown, no code fences, no greetings, no commentary.";
const PROMPT_INTERACTIVE: &str = "You are an inline terminal companion. Answer directly, no pleasantries. Keep it under 15 lines unless asked for more. Prefer commands, snippets and paths over background.";

#[derive(Parser)]
#[command(
    name = "prinia",
    version,
    about = "Call AI models from the command line like any other filter"
)]
struct Cli {
    /// The instruction; piped stdin is the data it applies to
    prompt: Option<String>,

    /// Provider to use (a built-in: anthropic, openai; or a [providers.NAME] from the config)
    #[arg(short, long)]
    provider: Option<String>,

    /// Fast model tier
    #[arg(short, long, conflicts_with_all = ["deep", "model"])]
    fast: bool,

    /// Reasoning model tier
    #[arg(short, long, conflicts_with = "model")]
    deep: bool,

    /// An exact model name
    #[arg(short, long)]
    model: Option<String>,

    /// Include earlier turns from this directory (`context` setting turns, default 5)
    #[arg(short = 'c', long, conflicts_with = "no_context")]
    context: bool,

    /// Send no earlier turns, even if the `context` setting is on
    #[arg(long)]
    no_context: bool,

    /// Let the model use tools to read and search files in this directory
    #[arg(short = 'a', long)]
    agent: bool,

    /// Shorthand for --agent --run: let the model run commands to answer
    #[arg(short = 'x', long = "do", conflicts_with = "no_agent")]
    do_it: bool,

    /// With --agent: allow creating and editing files
    #[arg(long)]
    write: bool,

    /// With --agent: allow running shell commands
    #[arg(long)]
    run: bool,

    /// With --run / -x: run every command without asking (read-only ones never ask)
    #[arg(short = 'y', long)]
    yes: bool,

    /// Use a saved instruction: `.prinia/templates/NAME` here or above, or `templates/NAME` in the config directory
    #[arg(short = 't', long, value_name = "NAME")]
    template: Option<String>,

    /// Run the instruction once per line of stdin, in parallel; answers are printed in input order
    #[arg(long, conflicts_with_all = ["agent", "do_it", "cmd"])]
    each: bool,

    /// With --each: how many calls run at once
    #[arg(short = 'j', long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=64))]
    jobs: u32,

    /// Answer with one shell command and nothing else (what the shell widget uses)
    #[arg(long, conflicts_with_all = ["agent", "do_it", "json", "check"])]
    cmd: bool,

    /// Don't use agent mode, even if the `agent` setting is on
    #[arg(long, conflicts_with = "agent")]
    no_agent: bool,

    /// Require the answer to be valid JSON; ask again if it is not
    #[arg(long, conflicts_with = "agent")]
    json: bool,

    /// Require this command to accept the answer on stdin (exit 0); ask again if it does not
    #[arg(long, value_name = "COMMAND", conflicts_with = "agent")]
    check: Option<String>,

    /// Call the model even if an identical call is in the history
    #[arg(long)]
    no_cache: bool,

    /// Save this turn to history (overrides `record = false`)
    #[arg(long, conflicts_with = "no_record")]
    record: bool,

    /// Do not save this turn to history
    #[arg(long)]
    no_record: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the effective settings and where each value comes from
    Config,
    /// Browse history (fzf when available), or show / delete turns
    History {
        #[command(flatten)]
        filter: history::FilterArgs,
        #[command(subcommand)]
        action: Option<HistoryCmd>,
    },
    /// Usage totals (calls, cache hits, tokens); fast, and never needs the history key
    Stats {
        /// Only this directory
        #[arg(long)]
        here: bool,
        /// Only the last N days: 0d is today, 7d a week (default: all time)
        #[arg(long)]
        since: Option<String>,
        /// Machine-readable output
        #[arg(long, conflicts_with = "format")]
        json: bool,
        /// Template: {calls} {cache_hits} {hit_rate} {input_tokens} {output_tokens} {tokens}
        #[arg(long)]
        format: Option<String>,
    },
    /// Show the context `-c` would send for this directory; `--summarize` builds the summaries first
    Context {
        /// Build and store summaries of older turns now (uses the fast tier of your provider)
        #[arg(long)]
        summarize: bool,
    },
    /// Find a local model server, pick a model for this machine and write the config
    Init,
    /// Print the shell integration to load: `eval "$(prinia shell zsh)"`
    Shell {
        #[arg(value_parser = ["zsh"])]
        shell: String,
    },
    /// Allow the `.priniarc` files here to turn on `run` and `write`
    Trust,
    /// Back up, restore or rotate the key that protects the history
    Key {
        #[command(subcommand)]
        action: KeyCmd,
    },
}

#[derive(Subcommand)]
enum HistoryCmd {
    /// Print one turn
    Show { id: i64 },
    /// Turns containing all of these words, with the matching line
    Search {
        #[arg(required = true)]
        words: Vec<String>,
        #[command(flatten)]
        filter: history::FilterArgs,
    },
    /// Ask a question about your history; a fast model reads excerpts of recent turns
    Ask {
        question: String,
        #[command(flatten)]
        filter: history::FilterArgs,
    },
    /// Let a model propose turns to delete from an instruction (asks before deleting)
    Agent {
        instruction: String,
        #[command(flatten)]
        filter: history::FilterArgs,
        /// Skip the confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Apply the `max_size` and `max_age` limits now and shrink the file
    Clean {
        /// Skip the confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Delete turns by id or filter (asks first)
    Rm {
        ids: Vec<i64>,
        #[command(flatten)]
        filter: history::FilterArgs,
        /// Skip the confirmation
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Print the key as 24 words, to write down or move to another machine
    Show,
    /// Restore a key from its 24 words (read from stdin)
    Import,
    /// Replace the key with a new one; the history stays readable
    Rotate,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("prinia: {e:#}");
        exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(&std::env::current_dir()?);
    let keychain = cfg.bool("keychain").unwrap_or(false);
    match &cli.command {
        Some(Cmd::Config) => show_config(&cfg),
        Some(Cmd::Context { summarize }) => {
            let provider = Provider::resolve(&cfg, cli.provider.as_deref())?;
            let model = provider.model(&cfg, Tier::Fast)?;
            let store = Store::open(keychain)?;
            let dir = store::dir_hash(&std::env::current_dir()?);
            let on = *summarize || cfg.bool("context_summary") == Some(true);
            let msgs = context::build(
                &store,
                &context::Params {
                    dir: &dir,
                    verbatim: (cfg.num("context").unwrap_or(0) as usize).max(5),
                    budget: cfg.num("context_chars").unwrap_or(16_000) as usize,
                    older: cfg.num("context_older").unwrap_or(30) as usize,
                    summary: on.then(|| context::Summary {
                        provider: &provider,
                        model: &model,
                        blocks: cfg.num("context_blocks").unwrap_or(4) as usize,
                    }),
                },
            )?;
            if msgs.is_empty() {
                println!("no history in this directory yet");
            }
            for (user, assistant) in msgs {
                println!("[you]\n{user}\n[model]\n{assistant}\n");
            }
            Ok(())
        }
        Some(Cmd::Init) => init::run(),
        Some(Cmd::Shell { .. }) => {
            print!("{}", include_str!("../scripts/prinia.zsh"));
            Ok(())
        }
        Some(Cmd::Trust) => {
            let done = config::trust(&std::env::current_dir()?)?;
            if done.is_empty() {
                bail!("no .priniarc here or above");
            }
            for p in done {
                println!("trusted {}", p.display());
            }
            Ok(())
        }
        Some(Cmd::Key { action }) => key_cmd(action, keychain),
        Some(Cmd::Stats {
            here,
            since,
            json,
            format,
        }) => stats::show(*here, since.as_deref(), *json, format.as_deref()),
        Some(Cmd::History {
            filter,
            action: None,
        }) => history::list(&Store::open(keychain)?, filter),
        Some(Cmd::History {
            action: Some(HistoryCmd::Show { id }),
            ..
        }) => history::show(&Store::open(keychain)?, *id),
        Some(Cmd::History {
            action: Some(HistoryCmd::Rm { ids, filter, yes }),
            ..
        }) => history::rm(&Store::open(keychain)?, ids, filter, *yes),
        Some(Cmd::History {
            action: Some(HistoryCmd::Search { words, filter }),
            ..
        }) => history::search(&Store::open(keychain)?, filter, words),
        Some(Cmd::History {
            action: Some(HistoryCmd::Ask { question, filter }),
            ..
        }) => curate::ask(&Store::open(keychain)?, &cfg, filter, question),
        Some(Cmd::History {
            action:
                Some(HistoryCmd::Agent {
                    instruction,
                    filter,
                    yes,
                }),
            ..
        }) => curate::agent(&Store::open(keychain)?, &cfg, filter, instruction, *yes),
        Some(Cmd::History {
            action: Some(HistoryCmd::Clean { yes }),
            ..
        }) => history::clean(&Store::open(keychain)?, &cfg, *yes),
        None if cli.each => each(&cli),
        None => ask(&cli, &cfg, keychain),
    }
}

/// The command line without the flags that only drive batching.
fn child_args() -> Vec<String> {
    let mut args = Vec::new();
    let mut raw = std::env::args().skip(1);
    while let Some(a) = raw.next() {
        match a.as_str() {
            "--each" => {}
            "-j" | "--jobs" => {
                raw.next();
            }
            _ if a.starts_with("--jobs=") => {}
            _ => args.push(a),
        }
    }
    args
}

/// One child `prinia` per input, `jobs` at a time, so caching, recording, checks and errors behave
/// exactly as for a single call. Results come back in input order as (text, succeeded).
fn fan_out(args: &[String], inputs: &[String], jobs: usize) -> Vec<(String, bool)> {
    let Ok(exe) = std::env::current_exe() else {
        return vec![("cannot find the prinia binary".into(), false); inputs.len()];
    };
    let results = std::sync::Mutex::new(vec![(String::new(), false); inputs.len()]);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..jobs.min(inputs.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(input) = inputs.get(i) else { return };
                    let r = std::process::Command::new(&exe)
                        .args(args)
                        // chunks are already small; never split them again
                        .env("PRINIA_MAX_INPUT", "0")
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                        .and_then(|mut c| {
                            let _ = c.stdin.take().map(|mut p| p.write_all(input.as_bytes()));
                            c.wait_with_output()
                        });
                    let r = match r {
                        Ok(o) if o.status.success() => (
                            String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
                            true,
                        ),
                        Ok(o) => (String::from_utf8_lossy(&o.stderr).trim().to_string(), false),
                        Err(e) => (e.to_string(), false),
                    };
                    results.lock().unwrap()[i] = r;
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    eprintln!("[prinia] {n}/{}", inputs.len());
                }
            });
        }
    });
    results.into_inner().unwrap()
}

fn each(cli: &Cli) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let lines: Vec<String> = input
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let mut failed = 0;
    for (i, (text, ok)) in fan_out(&child_args(), &lines, cli.jobs as usize)
        .into_iter()
        .enumerate()
    {
        if ok {
            let _ = writeln!(std::io::stdout(), "{text}");
        } else {
            failed += 1;
            eprintln!("[prinia] line {} failed: {text}", i + 1);
            let _ = writeln!(std::io::stdout());
        }
    }
    if failed > 0 {
        bail!("{failed} of {} lines failed", lines.len());
    }
    Ok(())
}

/// Pieces of at most `max` characters, cut at line ends where possible.
fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        while cur.chars().count() + rest.chars().count() > max {
            let room = max - cur.chars().count();
            if room == 0 {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            // a single over-long line is cut mid-line
            let cut = rest.char_indices().nth(room).map_or(rest.len(), |(i, _)| i);
            if cur.is_empty() {
                out.push(rest[..cut].to_string());
                rest = &rest[cut..];
            } else {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.push_str(rest);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Input over `max_input` characters: run the instruction on each piece, then once more over the
/// partial answers.
fn map_reduce(cli: &Cli, instruction: &str, payload: &str, max: usize) -> Result<()> {
    let pieces = chunks(payload, max);
    eprintln!(
        "[prinia] input is {} characters; processing it in {} parts",
        payload.chars().count(),
        pieces.len()
    );
    let partial = fan_out(&child_args(), &pieces, cli.jobs as usize);
    if let Some(i) = partial.iter().position(|(_, ok)| !ok) {
        bail!("part {} failed: {}", i + 1, partial[i].0);
    }
    let joined = partial
        .iter()
        .enumerate()
        .map(|(i, (t, _))| format!("--- part {} ---\n{t}", i + 1))
        .collect::<Vec<_>>()
        .join("\n\n");
    let final_instruction = format!(
        "The input was too long, so the instruction below was applied to consecutive parts of it separately; the results are given as the input. Combine them into one final answer to the instruction, without mentioning the parts.\n\nInstruction: {instruction}"
    );
    let mut args = child_args();
    // the final call carries the full instruction itself, so drop the template and the original one
    let mut skip = false;
    args.retain(|a| {
        if skip {
            skip = false;
            return false;
        }
        match a.as_str() {
            "-t" | "--template" => {
                skip = true;
                false
            }
            _ if a.starts_with("--template=") => false,
            _ => cli.prompt.as_deref() != Some(a.as_str()),
        }
    });
    args.push(final_instruction);
    let (text, ok) = fan_out(&args, &[joined], 1).remove(0);
    if !ok {
        bail!("{text}");
    }
    println!("{text}");
    Ok(())
}

fn ask(cli: &Cli, cfg: &Config, keychain: bool) -> Result<()> {
    let stdin_tty = std::io::stdin().is_terminal();
    let stdout_tty = std::io::stdout().is_terminal();
    let mut payload = String::new();
    if !stdin_tty {
        std::io::stdin().read_to_string(&mut payload)?;
    }
    let mut instruction = cli.prompt.clone().unwrap_or_default();
    if let Some(name) = &cli.template {
        let t = config::template(name, &std::env::current_dir()?)?;
        instruction = if instruction.is_empty() {
            t
        } else {
            format!("{t}\n\n{instruction}")
        };
    }
    if instruction.is_empty() && payload.trim().is_empty() {
        bail!("nothing to do. Usage: prinia \"question\"   or   cmd | prinia \"instruction\"");
    }

    let max_input = cfg.num("max_input").unwrap_or(0) as usize;
    if max_input > 0 && payload.chars().count() > max_input && !cli.cmd {
        return map_reduce(cli, &instruction, &payload, max_input);
    }
    if !stdin_tty && payload.trim().is_empty() {
        eprintln!("prinia: warning: stdin is empty, answering the instruction alone");
    }

    // Raw output whenever either end is a pipe.
    let piped = !stdin_tty || !stdout_tty || cli.cmd;
    let validate = cli.json || cli.check.is_some();
    // The setting only applies at a terminal; pipelines stay plain filters.
    let agent = cli.agent
        || cli.do_it
        || (!cli.no_agent && !validate && !piped && cfg.bool("agent").unwrap_or(false));
    let mode = if agent {
        "agent"
    } else if piped {
        "pipe"
    } else {
        "interactive"
    };
    let system = if cli.cmd {
        cmd_prompt()
    } else if piped {
        cfg.str("prompt_pipe").unwrap_or_else(|| PROMPT_PIPE.into())
    } else {
        cfg.str("prompt_interactive")
            .unwrap_or_else(|| PROMPT_INTERACTIVE.into())
    };

    let system = if cli.json {
        format!("{system} Respond with a single valid JSON value and nothing else.")
    } else {
        system
    };

    let provider = Provider::resolve(cfg, cli.provider.as_deref())?;
    let size = combine(&instruction, &payload).chars().count() as u64;
    let tier = if cli.fast {
        Tier::Fast
    } else if cli.deep {
        Tier::Deep
    } else if cli.model.is_some() {
        Tier::Regular
    } else if size < cfg.num("fast_under").unwrap_or(0) {
        Tier::Fast
    } else if cfg.num("deep_over").is_some_and(|n| size > n) {
        Tier::Deep
    } else {
        Tier::Regular
    };
    let model = match &cli.model {
        Some(m) => m.clone(),
        None => provider.model(cfg, tier)?,
    };
    let effort = provider.effort(cfg, tier);
    let max_tokens = cfg.num("max_tokens");
    let summary_model = provider
        .model(cfg, Tier::Fast)
        .unwrap_or_else(|_| model.clone());
    let user = combine(&instruction, &payload);

    let record = if cli.record {
        true
    } else if cli.no_record {
        false
    } else {
        cfg.bool("record").unwrap_or(true)
    };
    let cache_mode = cfg.str("cache").unwrap_or_else(|| "pipe".into());
    if !["pipe", "always", "off"].contains(&cache_mode.as_str()) {
        bail!("`cache` must be \"pipe\", \"always\" or \"off\"");
    }
    let use_cache =
        !cli.no_cache && !agent && (cache_mode == "always" || (cache_mode == "pipe" && piped));

    let cwd = std::env::current_dir()?;
    let dir = store::dir_hash(&cwd);
    let setting = cfg.num("context").unwrap_or(0) as usize;
    let ctx_turns = if cli.no_context {
        0
    } else if cli.context {
        if setting > 0 { setting } else { 5 }
    } else {
        setting
    };
    let store = if record || use_cache || ctx_turns > 0 {
        Some(Store::open(keychain)?)
    } else {
        None
    };
    let history = match &store {
        Some(s) if ctx_turns > 0 => context::build(
            s,
            &context::Params {
                dir: &dir,
                verbatim: ctx_turns,
                budget: cfg.num("context_chars").unwrap_or(16_000) as usize,
                older: cfg.num("context_older").unwrap_or(30) as usize,
                summary: (cfg.bool("context_summary") == Some(true)).then(|| context::Summary {
                    provider: &provider,
                    model: &summary_model,
                    blocks: cfg.num("context_blocks").unwrap_or(4) as usize,
                }),
            },
        )?,
        _ => Vec::new(),
    };
    let cache_key = store
        .as_ref()
        .map(|s| {
            s.cache_key(&[
                &provider.identity(),
                &model,
                &system,
                &user,
                &serde_json::json!(history).to_string(),
                effort.as_deref().unwrap_or(""),
                &max_tokens.map(|m| m.to_string()).unwrap_or_default(),
                if agent { "agent" } else { "" },
            ])
        })
        .unwrap_or_default();

    if use_cache && let Some(s) = &store {
        let ttl = history::age_secs(&cfg.str("cache_ttl").unwrap_or_else(|| "7d".into()))?;
        // a stored answer that fails the check is skipped, and the fresh one replaces it
        if let Some(hit) = s.cached(&cache_key, ttl, &dir)?
            && (!validate || check::passes(&hit, cli.json, cli.check.as_deref()))
        {
            let _ = std::io::stdout().write_all(hit.as_bytes());
            return Ok(());
        }
    }

    let started = Instant::now();
    let req = Request {
        model: &model,
        system: &system,
        user: &user,
        history: &history,
        tools: &[],
        follow: &[],
        effort: effort.as_deref(),
        max_tokens,
    };
    // A terminal gets the answer as it arrives; a pipe gets it whole, so a wrapping code fence
    // can be stripped. NOTE: filter the fence while streaming if piped latency ever matters.
    let mut show = |piece: &str| {
        if !piped && !validate {
            print!("{piece}");
            let _ = std::io::stdout().flush();
        }
    };
    // Checks need the whole answer, so nothing is printed (or saved) until it passes.
    let attempts = if validate {
        cfg.num("check_retries").unwrap_or(2) + 1
    } else {
        1
    };
    let mut tried = 0;
    let (reply, mut text) = loop {
        tried += 1;
        let reply = if agent {
            agent::run(
                &agent::Opts {
                    cfg,
                    provider: &provider,
                    model: &model,
                    effort: effort.as_deref(),
                    max_tokens,
                    user: &user,
                    write: cli.write || cfg.bool("write").unwrap_or(false),
                    run: cli.run || cli.do_it || cfg.bool("run").unwrap_or(false),
                    yes: cli.yes,
                },
                &mut show,
            )?
        } else {
            provider.complete(&req, &mut show)?
        };
        if reply.stop == "refusal" {
            bail!("the model declined this request");
        }
        let text = if piped || validate {
            strip_fence(&reply.text)
        } else {
            reply.text.clone()
        };
        if !validate || check::passes(&text, cli.json, cli.check.as_deref()) {
            break (reply, text);
        }
        if tried >= attempts {
            bail!("the answer failed the check {tried} time(s); nothing was printed or saved");
        }
        eprintln!("[prinia] the answer failed the check; asking again ({tried}/{attempts})");
    };
    if !text.ends_with('\n') {
        text.push('\n');
        if !piped && !validate {
            println!();
        }
    }
    if piped || validate {
        // a closed pipe (`| head`) is not an error
        let _ = std::io::stdout().write_all(text.as_bytes());
    }
    let _ = std::io::stdout().flush();
    let cut_off = matches!(reply.stop.as_str(), "max_tokens" | "length");
    if cut_off {
        eprintln!("[prinia] the answer was cut off at the output limit (raise `max_tokens`)");
    }

    // Bookkeeping happens after the output is out, so nothing downstream waits for it.
    if record && let Some(s) = &store {
        let (repo, branch) = git_info(&cwd);
        let meta = Meta {
            dir: &dir,
            provider: &provider.name,
            model: &reply.model,
            mode,
            tier: if cli.model.is_some() {
                "custom"
            } else {
                tier.name()
            },
            input_tokens: reply.input_tokens,
            output_tokens: reply.output_tokens,
            duration_ms: started.elapsed().as_millis() as u64,
            // an answer cut off at the limit stays in the history but never answers a later call
            cache_key: if cut_off { "" } else { &cache_key },
        };
        let content = Content {
            instruction,
            payload,
            response: text,
            cwd: cwd.to_string_lossy().into_owned(),
            repo,
            branch,
        };
        if let Err(e) = s.record(&meta, &content) {
            eprintln!("[prinia] history not saved: {e:#}");
        } else {
            history::enforce_limits(s, cfg);
        }
    }
    Ok(())
}

fn combine(instruction: &str, payload: &str) -> String {
    match (instruction.is_empty(), payload.is_empty()) {
        (false, false) => format!("{instruction}\n\n{payload}"),
        (false, true) => instruction.to_string(),
        _ => payload.to_string(),
    }
}

/// The system prompt for `--cmd`: names the shell and OS, and the modern tools actually installed.
fn cmd_prompt() -> String {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell = shell
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("sh");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tools: Vec<&str> = ["rg", "fd", "jq", "yq"]
        .into_iter()
        .filter(|t| std::env::split_paths(&path).any(|d| d.join(t).is_file()))
        .collect();
    format!(
        "Turn the user's request into ONE shell command for {shell} on {}. Output only the command on a single line: no explanation, no markdown, no code fences. Combine steps with | or &&. Installed tools you may prefer when they fit: {}. If the request is not something a command can do, output a single line starting with # that says why.",
        std::env::consts::OS,
        if tools.is_empty() {
            "none".into()
        } else {
            tools.join(", ")
        }
    )
}

fn strip_fence(s: &str) -> String {
    let t = s.trim();
    if let Some(body) = t.strip_prefix("```").and_then(|r| r.strip_suffix("```"))
        && let Some((_lang, inner)) = body.split_once('\n')
        && !inner.contains("```")
    {
        return format!("{}\n", inner.trim_end());
    }
    s.to_string()
}

fn git_info(cwd: &Path) -> (Option<String>, Option<String>) {
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let repo = run(&["rev-parse", "--show-toplevel"]).and_then(|p| {
        Path::new(&p)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    });
    (repo, run(&["symbolic-ref", "--short", "-q", "HEAD"]))
}

fn key_cmd(action: &KeyCmd, keychain: bool) -> Result<()> {
    let missing =
        || anyhow!("there is no key yet; one is created the first time history is written");
    match action {
        KeyCmd::Show => {
            let k = key::load(keychain)?.ok_or_else(missing)?;
            println!("{}", key::to_words(&k)?);
            eprintln!(
                "\nAnyone with these words and your history file can read it. Keep them somewhere safe."
            );
        }
        KeyCmd::Import => {
            let mut words = String::new();
            std::io::stdin().read_to_string(&mut words)?;
            let k = key::from_words(&words)?;
            if !Store::key_fits(&store::path()?, &k)? {
                bail!("that key does not unlock the existing history; nothing was changed");
            }
            key::save(&k, keychain)?;
            println!("key imported");
        }
        KeyCmd::Rotate => {
            key::load(keychain)?.ok_or_else(missing)?;
            let store = Store::open(keychain)?;
            let new = key::random()?;
            store.rewrap(&new, keychain)?;
            println!(
                "key rotated; the old key no longer works. Back up the new one with `prinia key show`."
            );
        }
    }
    Ok(())
}

fn show_config(cfg: &Config) -> Result<()> {
    let files: Vec<_> = cfg.files.iter().map(|p| p.display().to_string()).collect();
    println!(
        "# files (lowest to highest priority): {}",
        if files.is_empty() {
            "none".into()
        } else {
            files.join(", ")
        }
    );
    println!(
        "# global: {}",
        config::global_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    println!("# order: flag > .priniarc (directory) > env PRINIA_<KEY> > global file > default");
    for (key, _) in config::KEYS {
        let (value, source) = cfg.describe(key);
        let value: String = value.chars().take(60).collect();
        println!("{key:<20} {value:<62} [{source}]");
    }
    println!(
        "# providers: anthropic, openai (built in){}",
        cfg.provider_names()
            .iter()
            .map(|n| format!(", {n}"))
            .collect::<String>()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cut_at_lines_and_lose_nothing() {
        let text = "aaaa\nbbbb\ncc\n";
        let c = chunks(text, 10);
        assert_eq!(c, ["aaaa\nbbbb\n", "cc\n"]);
        assert_eq!(chunks("xxxxxxxxxxxx", 5).concat(), "xxxxxxxxxxxx");
        assert!(
            chunks("é".repeat(7).as_str(), 3)
                .iter()
                .all(|p| p.chars().count() <= 3)
        );
        assert!(chunks("", 5).is_empty());
    }

    #[test]
    fn combine_and_fence() {
        assert_eq!(combine("do x", "data"), "do x\n\ndata");
        assert_eq!(combine("", "data"), "data");
        assert_eq!(strip_fence("```json\n{\"a\":1}\n```\n"), "{\"a\":1}\n");
        assert_eq!(strip_fence("plain\n"), "plain\n");
        assert_eq!(strip_fence("a\n```\nx\n```\nb"), "a\n```\nx\n```\nb");
    }
}
