//! Finding the turns of a history that match some words: used by `history ask` and by `-c`.
use crate::store::Turn;

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

/// The meaningful lowercase words of a question, without repeats.
pub fn keywords(q: &str) -> Vec<String> {
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

/// Up to `k` turns scoring at least `min` for the keywords (a prompt match counts 3, input 2,
/// answer 1, per keyword), best first, newest first among equals. A word that appears in more than
/// half of a larger set says nothing about any one turn and is ignored.
pub fn rank<'a>(turns: &'a [Turn], kws: &[String], k: usize, min: usize) -> Vec<&'a Turn> {
    let low: Vec<[String; 3]> = turns
        .iter()
        .map(|t| {
            let c = &t.content;
            [
                c.instruction.to_lowercase(),
                c.payload.to_lowercase(),
                c.response.to_lowercase(),
            ]
        })
        .collect();
    let common = |w: &String| {
        turns.len() >= 4
            && low
                .iter()
                .filter(|l| l.iter().any(|x| x.contains(w.as_str())))
                .count()
                * 2
                > turns.len()
    };
    let kws: Vec<&String> = kws.iter().filter(|w| !common(w)).collect();
    let mut scored: Vec<(usize, &Turn)> = turns
        .iter()
        .zip(&low)
        .map(|(t, [i, p, r])| {
            let s = kws
                .iter()
                .map(|w| {
                    3 * i.contains(w.as_str()) as usize
                        + 2 * p.contains(w.as_str()) as usize
                        + r.contains(w.as_str()) as usize
                })
                .sum();
            (s, t)
        })
        .collect();
    scored.retain(|(s, _)| *s >= min.max(1));
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
    scored.into_iter().take(k).map(|(_, t)| t).collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn turn_with(id: i64, instruction: &str, response: &str) -> Turn {
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
    fn keywords_drop_filler_and_repeats() {
        assert_eq!(
            keywords("what was the issue with the API request last week, API?"),
            ["issue", "api", "request"]
        );
    }

    #[test]
    fn rank_prefers_prompt_matches_and_honours_the_minimum() {
        let turns = [
            turn_with(3, "unrelated", "the api answered"),
            turn_with(2, "why does the api request fail", "timeout"),
            turn_with(1, "other", "other"),
        ];
        let kws = keywords("api request");
        let ids = |min| -> Vec<i64> { rank(&turns, &kws, 2, min).iter().map(|t| t.id).collect() };
        assert_eq!(ids(1), [2, 3]);
        assert_eq!(ids(2), [2], "a lone answer match is too weak");
        assert!(rank(&turns, &keywords("zebra"), 2, 1).is_empty());
    }

    #[test]
    fn words_in_most_turns_are_ignored() {
        let turns = [
            turn_with(4, "question about pipes", "a"),
            turn_with(3, "question about git", "b"),
            turn_with(2, "question about make", "c"),
            turn_with(1, "question about the payments api", "d"),
        ];
        let ids: Vec<i64> = rank(&turns, &keywords("question payments"), 3, 2)
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, [1], "`question` is in every turn, `payments` in one");
    }
}
