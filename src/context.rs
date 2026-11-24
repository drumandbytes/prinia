//! Earlier turns from this directory's history, sent as context. Three tiers by age:
//! the newest turns word for word, then one line per older question, then (opt-in) model-written
//! summaries of still older blocks of turns. Summaries are derived from the history, cached by the
//! exact turns they cover, and dropped whenever anything is deleted.
use crate::provider::{Provider, Request};
use crate::retrieve::{keywords, rank};
use crate::store::{Filter, Store, Turn};
use anyhow::Result;

const USER_CAP: usize = 2_000;
const REPLY_CAP: usize = 4_000;
/// Turns are grouped into blocks by `id / BLOCK`; a block is summarized once, when no new turn can
/// land in it any more.
const BLOCK: i64 = 25;
const LIST_LIMIT: usize = 1_000;

pub struct Params<'a> {
    pub dir: &'a str,
    pub verbatim: usize,
    pub budget: usize,
    /// How many turns beyond the verbatim ones are listed as one-line questions.
    pub older: usize,
    /// The new question; older turns that match its words are included in full.
    pub query: &'a str,
    pub relevant: usize,
    /// Which turns this call may show to its provider (see `Provider::may_see`).
    pub visible: &'a dyn Fn(&Turn) -> bool,
    /// Summarize older blocks with this provider and model (the call's own, so privacy rules hold).
    pub summary: Option<Summary<'a>>,
}

pub struct Summary<'a> {
    pub provider: &'a Provider,
    pub model: &'a str,
    pub blocks: usize,
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n[...cut]")
}

fn user_text(t: &Turn) -> String {
    let c = &t.content;
    match (c.instruction.is_empty(), c.payload.is_empty()) {
        (false, false) => format!("{}\n\n{}", c.instruction, c.payload),
        (false, true) => c.instruction.clone(),
        _ => c.payload.clone(),
    }
}

fn question(t: &Turn) -> String {
    let first = if t.content.instruction.is_empty() {
        &t.content.payload
    } else {
        &t.content.instruction
    };
    let line = first.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let short: String = line.trim().chars().take(160).collect();
    format!("- {short}")
}

fn note(title: &str, body: &str) -> (String, String) {
    (format!("{title}\n{body}"), "Understood.".to_string())
}

/// Turns summarized per block are cut so a block fits any model.
fn block_text(turns: &[&Turn]) -> String {
    turns
        .iter()
        .map(|t| {
            format!(
                "Q: {}\nA: {}",
                cut(&user_text(t), 300),
                cut(t.content.response.trim(), 300)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn summarize(store: &Store, s: &Summary, block: &[&Turn]) -> Result<String> {
    let ids = block
        .iter()
        .map(|t| t.id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let key = store.cache_key(&["summary-v1", &s.provider.identity(), s.model, &ids]);
    if let Some(hit) = store.summary_get(&key)? {
        return Ok(hit);
    }
    let text = block_text(block);
    let req = Request {
        model: s.model,
        system: "Summarize this stretch of a user's terminal question-and-answer history in under 120 words. Keep names, commands, file paths, decisions and open questions. No preamble.",
        user: &text,
        history: &[],
        tools: &[],
        follow: &[],
        effort: None,
        max_tokens: Some(400),
    };
    let reply = s.provider.complete(&req, &mut |_| {})?;
    let out = reply.text.trim().to_string();
    if !out.is_empty() && reply.stop != "max_tokens" && reply.stop != "length" {
        store.summary_put(&key, &out)?;
    }
    Ok(out)
}

/// Messages to send before the new one, oldest first.
pub fn build(store: &Store, p: &Params) -> Result<Vec<(String, String)>> {
    let filter = Filter {
        dir: Some(p.dir.to_string()),
        ..Filter::default()
    };
    let limit = if p.summary.is_some() || (p.relevant > 0 && !p.query.is_empty()) {
        LIST_LIMIT
    } else {
        p.verbatim + p.older
    };
    let mut turns = store.list(&filter, limit)?; // newest first
    turns.retain(|t| (p.visible)(t));

    // tier 1: newest turns word for word, within the character budget
    let mut used = 0;
    let mut recent = Vec::new();
    for t in turns.iter().take(p.verbatim) {
        let pair = (
            cut(&user_text(t), USER_CAP),
            cut(t.content.response.trim_end(), REPLY_CAP),
        );
        let size = pair.0.chars().count() + pair.1.chars().count();
        if used + size > p.budget {
            break;
        }
        used += size;
        recent.push(pair);
    }
    recent.reverse();

    // tier 2: one line per older question
    let rest = &turns[recent.len()..];
    let asked = &rest[..p.older.min(rest.len())];
    let beyond = &rest[asked.len()..];

    // retrieval: older turns about what is being asked now, in full, within half the budget
    let mut related = Vec::new();
    let mut related_ids = Vec::new();
    if p.relevant > 0 && !p.query.is_empty() {
        let kws = keywords(&p.query.chars().take(500).collect::<String>());
        let mut used = 0;
        for t in rank(rest, &kws, p.relevant, 2) {
            let pair = (
                cut(&user_text(t), USER_CAP),
                cut(t.content.response.trim_end(), REPLY_CAP),
            );
            let size = pair.0.chars().count() + pair.1.chars().count();
            if used + size > p.budget / 2 {
                continue;
            }
            used += size;
            related_ids.push(t.id);
            related.push(pair);
        }
        related.reverse(); // oldest first, like the rest of the context
    }

    let mut out = Vec::new();
    // tier 3: summaries of complete older blocks, newest blocks first, at most `blocks` of them
    if let Some(s) = &p.summary {
        let edge = rest.first().map_or(i64::MAX, |_| {
            turns[..recent.len() + asked.len()]
                .iter()
                .map(|t| t.id)
                .min()
                .unwrap_or(i64::MAX)
        }) / BLOCK;
        let mut by_block: std::collections::BTreeMap<i64, Vec<&Turn>> = Default::default();
        for t in beyond.iter().filter(|t| t.id / BLOCK < edge) {
            by_block.entry(t.id / BLOCK).or_default().push(t);
        }
        let mut lines = Vec::new();
        for (_, mut block) in by_block.into_iter().rev().take(s.blocks) {
            block.reverse(); // oldest first
            match summarize(store, s, &block) {
                Ok(t) if !t.is_empty() => lines.push(format!("- {}", t.replace('\n', " "))),
                Ok(_) => {}
                Err(e) => eprintln!("[prinia] could not summarize older turns: {e}"),
            }
        }
        if !lines.is_empty() {
            lines.reverse(); // oldest first
            out.push(note(
                "Summaries of older exchanges in this directory (oldest first), for background:",
                &lines.join("\n"),
            ));
        }
    }
    let lines: Vec<String> = asked
        .iter()
        .rev()
        .filter(|t| !related_ids.contains(&t.id))
        .map(question)
        .collect();
    if !lines.is_empty() {
        out.push(note(
            "Earlier questions asked in this directory (oldest first), for background:",
            &lines.join("\n"),
        ));
    }
    out.extend(related);
    out.extend(recent);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_long_text() {
        assert_eq!(cut("abc", 5), "abc");
        assert_eq!(cut("abcdef", 3), "abc\n[...cut]");
    }
}
