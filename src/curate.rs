//! `history ask` and `history agent`: a model reads excerpts of the history to answer a question
//! about it or to pick turns to delete. These calls are not recorded.
use crate::config::Config;
use crate::history::{FilterArgs, confirm, oneline};
use crate::provider::{Provider, Request, Tier};
use crate::store::{Store, Turn};
use anyhow::{Result, bail};
use std::io::Write;

const ASK_SYSTEM: &str = "You answer questions about a user's command-line AI history. The entries are the best matches for the question, not the whole history. Each is `id | date | place | Q: prompt | A: answer`. Answer briefly and cite entry ids as #id. If the entries do not contain the answer, say so instead of guessing.";
const AGENT_SYSTEM: &str = "You help clean up a user's command-line AI history. Each entry is `id | date | place | Q: prompt | A: answer`. Follow the user's instruction and reply with ONLY a JSON array of the integer ids of the entries to delete, for example [3,8]. Use only ids that are listed. If nothing matches, reply [].";

fn listing<'a>(turns: impl IntoIterator<Item = &'a Turn>, q: usize, a: usize) -> String {
    turns
        .into_iter()
        .map(|t| {
            let c = &t.content;
            let place = c.repo.as_deref().unwrap_or(&c.cwd);
            format!(
                "{} | {} | {} | Q: {} | A: {}",
                t.id,
                t.when,
                place,
                oneline(&crate::history::label(t), q),
                oneline(&c.response, a)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn call(cfg: &Config, tier: Tier, system: &str, user: &str, show: bool) -> Result<String> {
    let provider = Provider::resolve(cfg, None)?;
    let model = provider.model(cfg, tier)?;
    let effort = provider.effort(cfg, tier);
    let req = Request {
        model: &model,
        system,
        user,
        history: &[],
        tools: &[],
        follow: &[],
        effort: effort.as_deref(),
        max_tokens: cfg.num("max_tokens"),
    };
    let reply = provider.complete(&req, &mut |piece| {
        if show {
            print!("{piece}");
            let _ = std::io::stdout().flush();
        }
    })?;
    Ok(reply.text)
}

fn scan(store: &Store, f: &FilterArgs, cfg: &Config) -> Result<Vec<Turn>> {
    let turns = store.list(
        &f.filter()?,
        cfg.num("history_scan").unwrap_or(200) as usize,
    )?;
    if turns.is_empty() {
        bail!("the history has no matching turns");
    }
    Ok(turns)
}

const FILLER: &[&str] = &[
    "the",
    "and",
    "was",
    "were",
    "what",
    "which",
    "who",
    "how",
    "why",
    "when",
    "where",
    "did",
    "does",
    "for",
    "with",
    "about",
    "that",
    "this",
    "these",
    "those",
    "have",
    "had",
    "has",
    "from",
    "into",
    "you",
    "your",
    "our",
    "can",
    "could",
    "would",
    "should",
    "last",
    "ago",
    "week",
    "weeks",
    "day",
    "days",
    "month",
    "months",
    "year",
    "yesterday",
    "today",
    "recently",
    "tell",
    "show",
    "find",
    "ask",
    "asked",
    "remember",
    "again",
    "there",
    "then",
    "than",
];

/// Time words in a question as (newer than, older than) seconds. Generous on purpose: "last week"
/// covers the past two weeks, and the keywords do the narrowing.
fn parse_when(q: &str) -> (Option<i64>, Option<i64>) {
    const D: i64 = 86_400;
    let q = q.to_lowercase();
    let w: Vec<&str> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    for pair in w.windows(3) {
        if let (Ok(n), Some(unit), "ago") = (
            pair[0].parse::<i64>(),
            ["hour", "day", "week", "month"]
                .iter()
                .position(|u| pair[1].trim_end_matches('s') == *u),
            pair[2],
        ) {
            let u = [3600, D, 7 * D, 30 * D][unit];
            return (
                Some((n + 1) * u),
                Some((n - 1).max(0) * u).filter(|o| *o > 0),
            );
        }
    }
    let has = |p: &str| q.contains(p);
    if has("last week") || has("past week") {
        (Some(14 * D), None)
    } else if has("last month") || has("past month") {
        (Some(62 * D), None)
    } else if has("yesterday") {
        (Some(2 * D), None)
    } else if has("today") {
        (Some(D), None)
    } else if has("this week") || has("recently") {
        (Some(7 * D), None)
    } else {
        (None, None)
    }
}

fn keywords(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in q
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
    {
        if w.chars().count() >= 3 && !FILLER.contains(&w) && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// The `k` turns that match the keywords best (prompt counts most, then input, then answer),
/// best first; newest first among equals. With no keyword hit, simply the newest `k`.
fn rank<'a>(turns: &'a [Turn], kws: &[String], k: usize) -> Vec<&'a Turn> {
    let score = |t: &Turn| -> usize {
        let c = &t.content;
        let (i, p, r) = (
            c.instruction.to_lowercase(),
            c.payload.to_lowercase(),
            c.response.to_lowercase(),
        );
        kws.iter()
            .map(|w| {
                3 * i.contains(w.as_str()) as usize
                    + 2 * p.contains(w.as_str()) as usize
                    + r.contains(w.as_str()) as usize
            })
            .sum()
    };
    let mut scored: Vec<(usize, &Turn)> = turns.iter().map(|t| (score(t), t)).collect();
    if scored.iter().all(|(s, _)| *s == 0) {
        return turns.iter().take(k).collect();
    }
    scored.retain(|(s, _)| *s > 0);
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
    scored.into_iter().take(k).map(|(_, t)| t).collect()
}

/// Answer a question about the history, using the fast tier: find the matching turns across the
/// whole history (time words and keywords from the question, or `--since` / `--older`), then let
/// the model answer from them.
pub fn ask(store: &Store, cfg: &Config, f: &FilterArgs, question: &str) -> Result<()> {
    let mut filter = f.filter()?;
    let (since, older) = parse_when(question);
    let mut guessed = false;
    if filter.since.is_none() && filter.older.is_none() && (since.is_some() || older.is_some()) {
        filter.since = since;
        filter.older = older;
        guessed = true;
    }
    let mut turns = store.list(&filter, 20_000)?;
    if turns.is_empty() && guessed {
        eprintln!("[prinia] nothing in that time range; searching all of the history");
        filter.since = None;
        filter.older = None;
        turns = store.list(&filter, 20_000)?;
    }
    if turns.is_empty() {
        bail!("the history has no matching turns");
    }
    let kws = keywords(question);
    let k = cfg.num("ask_turns").unwrap_or(15) as usize;
    let best = rank(&turns, &kws, k);
    eprintln!(
        "[prinia] {} of {} turns{}{}",
        best.len(),
        turns.len(),
        if kws.is_empty() {
            String::new()
        } else {
            format!(" for: {}", kws.join(", "))
        },
        if guessed {
            " (time range taken from the question)"
        } else {
            ""
        }
    );
    let listing = listing(
        best.iter().copied(),
        cfg.num("excerpt_prompt").unwrap_or(400) as usize,
        cfg.num("excerpt_reply").unwrap_or(1000) as usize,
    );
    let text = call(
        cfg,
        Tier::Fast,
        ASK_SYSTEM,
        &format!("{listing}\n\nQuestion: {question}"),
        true,
    )?;
    if !text.ends_with('\n') {
        println!();
    }
    eprintln!(
        "[prinia] turns used: {}   (prinia history show ID)",
        best.iter()
            .map(|t| format!("#{}", t.id))
            .collect::<Vec<_>>()
            .join(" ")
    );
    Ok(())
}

/// The ids the model picked, restricted to turns that were offered.
fn picked(reply: &str, offered: &[Turn]) -> Vec<i64> {
    let (Some(a), Some(b)) = (reply.find('['), reply.rfind(']')) else {
        return Vec::new();
    };
    if a > b {
        return Vec::new();
    }
    let ids: Vec<i64> = serde_json::from_str(&reply[a..=b]).unwrap_or_default();
    offered
        .iter()
        .map(|t| t.id)
        .filter(|id| ids.contains(id))
        .collect()
}

/// Let the model propose deletions for an instruction; nothing is deleted without confirmation.
pub fn agent(
    store: &Store,
    cfg: &Config,
    f: &FilterArgs,
    instruction: &str,
    yes: bool,
) -> Result<()> {
    let turns = scan(store, f, cfg)?;
    let user = format!(
        "{}\n\nInstruction: {instruction}",
        listing(
            &turns,
            cfg.num("excerpt_prompt").unwrap_or(200) as usize,
            cfg.num("excerpt_reply").unwrap_or(400) as usize
        )
    );
    let reply = call(cfg, Tier::Regular, AGENT_SYSTEM, &user, false)?;
    let ids = picked(&reply, &turns);
    if ids.is_empty() {
        println!("nothing to delete");
        return Ok(());
    }
    for t in turns.iter().filter(|t| ids.contains(&t.id)).take(15) {
        println!("  #{} {} {}", t.id, t.when, crate::history::label(t));
    }
    if ids.len() > 15 {
        println!("  ... and {} more", ids.len() - 15);
    }
    if !confirm(
        &format!(
            "The model proposes deleting {} turn(s). Delete? This cannot be undone.",
            ids.len()
        ),
        yes,
    )? {
        println!("aborted");
        return Ok(());
    }
    println!("deleted {}", store.delete(&ids)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_offered_ids_are_picked() {
        let turn = |id| Turn {
            id,
            when: String::new(),
            provider: String::new(),
            model: String::new(),
            mode: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            content: Default::default(),
        };
        let offered = [turn(1), turn(2), turn(3)];
        assert_eq!(picked("```json\n[3, 1, 99]\n```", &offered), [1, 3]);
        assert!(picked("no idea", &offered).is_empty());
        assert!(picked("x] y [3", &offered).is_empty()); // ] before [ must not panic
        assert!(picked("[\"x\"]", &offered).is_empty());
    }

    fn turn_with(id: i64, instruction: &str, response: &str) -> Turn {
        let mut t = Turn {
            id,
            when: String::new(),
            provider: String::new(),
            model: String::new(),
            mode: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            content: Default::default(),
        };
        t.content.instruction = instruction.into();
        t.content.response = response.into();
        t
    }

    #[test]
    fn time_words_and_keywords_come_from_the_question() {
        const D: i64 = 86_400;
        assert_eq!(
            parse_when("what was the API issue last week"),
            (Some(14 * D), None)
        );
        assert_eq!(parse_when("what did I ask yesterday"), (Some(2 * D), None));
        assert_eq!(
            parse_when("the retry bug 3 days ago"),
            (Some(4 * D), Some(2 * D))
        );
        assert_eq!(parse_when("how do pipes work"), (None, None));
        assert_eq!(
            keywords("what was the issue with the API request last week"),
            ["issue", "api", "request"]
        );
    }

    #[test]
    fn rank_prefers_prompt_matches_and_falls_back_to_newest() {
        let turns = [
            turn_with(3, "unrelated", "the api answered"),
            turn_with(2, "why does the api request fail", "timeout"),
            turn_with(1, "other", "other"),
        ];
        let kws = keywords("api request");
        let ids: Vec<i64> = rank(&turns, &kws, 2).iter().map(|t| t.id).collect();
        assert_eq!(ids, [2, 3]);
        let none: Vec<i64> = rank(&turns, &keywords("zebra"), 2)
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(none, [3, 2], "no hit: the newest turns");
    }
}
