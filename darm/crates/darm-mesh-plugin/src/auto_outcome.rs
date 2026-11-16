//! Automatic outcome reports for recalled memories.
//!
//! Outcome learning needs to be told whether a recalled memory helped. This
//! module infers that from the conversation itself, in two steps:
//!
//! 1. **Was it used?** After a turn, a recalled memory counts as *used* when
//!    the answer contains enough of the memory's distinctive words (words
//!    the question did not already contain). Unused memories get no report.
//! 2. **How did the user react?** The used memories wait for the next turn
//!    of the same conversation (recognised by the previous answer appearing
//!    in the request history). A correction ("that's wrong", ...) or the
//!    same question asked again reports `Hurt`; any other follow-up reports
//!    `Helped`. No follow-up within the window: no report.
//!
//! Limits, stated plainly: "used" is word overlap, not proof the memory was
//! right; a confidently wrong memory the user accepts is scored `Helped`.
//! The correction phrases are English. Anyone who can chat through this node
//! can steer the scores of memories recalled for their own turns, which is
//! why this is off by default on a public mesh. Reports stay local and are
//! bounded by one report per recall, as with manual reports.

use darm_core::Hash32;
use std::collections::{BTreeSet, VecDeque};

/// How long a turn waits for its follow-up.
pub const FOLLOW_UP_WINDOW_MS: u64 = 30 * 60 * 1_000;
/// Turns kept waiting at once.
const MAX_PENDING: usize = 256;
/// A memory is "used" when the answer has at least this many of its
/// distinctive words, and at least this share of them (per mille).
const MIN_SHARED_WORDS: usize = 3;
const MIN_SHARED_MILLI: usize = 300;
/// Distinctive words considered per memory (bounds the work per turn).
const MAX_WORDS: usize = 256;
/// Two questions are "the same" at or above this word-set overlap (per mille).
const REASK_MILLI: usize = 800;

const CORRECTIONS: [&str; 16] = [
    "that's wrong",
    "that is wrong",
    "thats wrong",
    "that's not right",
    "that is not right",
    "that's incorrect",
    "that is incorrect",
    "not correct",
    "you're wrong",
    "you are wrong",
    "wrong answer",
    "that's not what i",
    "that is not what i",
    "not what i asked",
    "doesn't answer",
    "does not answer",
];

/// Lowercased words of four or more letters or digits.
fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_lowercase)
        .take(4 * MAX_WORDS)
        .collect()
}

/// Whether `answer` draws on `memory` beyond what `query` already said.
pub fn grounded(query: &str, memory: &str, answer: &str) -> bool {
    let asked = words(query);
    let distinctive: Vec<String> = words(memory)
        .into_iter()
        .filter(|w| !asked.contains(w))
        .take(MAX_WORDS)
        .collect();
    if distinctive.len() < MIN_SHARED_WORDS {
        return false;
    }
    let said = words(answer);
    let shared = distinctive.iter().filter(|w| said.contains(*w)).count();
    shared >= MIN_SHARED_WORDS && shared * 1_000 >= distinctive.len() * MIN_SHARED_MILLI
}

/// Whether a follow-up message rejects the previous answer.
pub fn rejects(previous_query: &str, follow_up: &str) -> bool {
    let lower = follow_up.to_lowercase().replace('\u{2019}', "'");
    if CORRECTIONS.iter().any(|phrase| lower.contains(phrase)) {
        return true;
    }
    let (a, b) = (words(previous_query), words(follow_up));
    if a.len() < 2 || b.len() < 2 {
        return false;
    }
    let shared = a.intersection(&b).count();
    let union = a.union(&b).count();
    shared * 1_000 >= union * REASK_MILLI
}

fn answer_key(namespace: &str, answer: &str) -> Hash32 {
    let mut bytes = namespace.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend(
        answer
            .split_whitespace()
            .flat_map(|w| w.bytes().chain(*b" ")),
    );
    Hash32::digest("auto-outcome-answer", &bytes)
}

struct Waiting {
    key: Hash32,
    query: String,
    used: Vec<Hash32>,
    at_ms: u64,
}

/// What a follow-up decided for the memories used in the previous answer.
#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    pub helped: bool,
    pub memories: Vec<Hash32>,
}

#[derive(Default)]
pub struct AutoOutcomes {
    waiting: VecDeque<Waiting>,
    pub reported_helped: u64,
    pub reported_hurt: u64,
}

impl AutoOutcomes {
    /// After a turn: hold the memories the answer used until the follow-up.
    pub fn after_turn(
        &mut self,
        namespace: &str,
        query: &str,
        answer: &str,
        used: Vec<Hash32>,
        now_ms: u64,
    ) {
        if used.is_empty() {
            return;
        }
        self.expire(now_ms);
        if self.waiting.len() >= MAX_PENDING {
            self.waiting.pop_front();
        }
        self.waiting.push_back(Waiting {
            key: answer_key(namespace, answer),
            query: query.chars().take(2_000).collect(),
            used,
            at_ms: now_ms,
        });
    }

    /// Before a turn: if `previous_answer` (the last assistant message in the
    /// request) is one this node produced recently, judge it by `follow_up`.
    pub fn on_follow_up(
        &mut self,
        namespace: &str,
        previous_answer: &str,
        follow_up: &str,
        now_ms: u64,
    ) -> Option<Verdict> {
        self.expire(now_ms);
        let key = answer_key(namespace, previous_answer);
        let position = self.waiting.iter().position(|w| w.key == key)?;
        let waiting = self.waiting.remove(position)?;
        Some(Verdict {
            helped: !rejects(&waiting.query, follow_up),
            memories: waiting.used,
        })
    }

    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    fn expire(&mut self, now_ms: u64) {
        while self
            .waiting
            .front()
            .is_some_and(|w| now_ms.saturating_sub(w.at_ms) > FOLLOW_UP_WINDOW_MS)
        {
            self.waiting.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMORY: &str = "User: where are the runbooks?\nAssistant: The runbooks live in the ops repository under docs/incident, maintained by the platform team.";

    #[test]
    fn grounding_needs_the_memorys_own_words_in_the_answer() {
        let query = "where are the runbooks?";
        assert!(grounded(
            query,
            MEMORY,
            "They are in the ops repository under docs/incident; the platform team maintains them."
        ));
        // Repeating the question's words is not use of the memory.
        assert!(!grounded(
            query,
            MEMORY,
            "I do not know where the runbooks are."
        ));
        assert!(!grounded(
            query,
            MEMORY,
            "Bananas are yellow and rich in potassium."
        ));
        // Too little distinctive content to judge.
        assert!(!grounded(
            "runbooks location",
            "runbooks location ok",
            "runbooks location ok"
        ));
    }

    #[test]
    fn corrections_and_repeats_reject_other_follow_ups_do_not() {
        let query = "where are the incident runbooks kept?";
        assert!(rejects(query, "No, that's wrong. They moved last year."));
        assert!(rejects(query, "That\u{2019}s not right"));
        assert!(rejects(query, "Where are the incident runbooks kept"));
        assert!(!rejects(query, "Thanks. Who maintains them?"));
        assert!(!rejects(query, "ok"));
    }

    #[test]
    fn follow_ups_match_by_answer_within_the_window_once() {
        let mut auto = AutoOutcomes::default();
        let id = Hash32::digest("m", b"1");
        auto.after_turn("team", "q one two", "the  answer\ntext", vec![id], 1_000);
        auto.after_turn("team", "unused", "other", Vec::new(), 1_000);
        assert_eq!(auto.waiting(), 1);
        // Another namespace or another answer does not match.
        assert!(
            auto.on_follow_up("other", "the answer text", "thanks", 2_000)
                .is_none()
        );
        assert!(
            auto.on_follow_up("team", "different", "thanks", 2_000)
                .is_none()
        );
        // Whitespace differences in the echoed answer do not matter.
        let verdict = auto
            .on_follow_up("team", "the answer text", "thanks", 2_000)
            .unwrap();
        assert_eq!(
            verdict,
            Verdict {
                helped: true,
                memories: vec![id]
            }
        );
        assert!(
            auto.on_follow_up("team", "the answer text", "thanks", 2_000)
                .is_none()
        );

        auto.after_turn("team", "q one two", "late", vec![id], 1_000);
        let late = 1_000 + FOLLOW_UP_WINDOW_MS + 1;
        assert!(
            auto.on_follow_up("team", "late", "that's wrong", late)
                .is_none()
        );
        assert_eq!(auto.waiting(), 0);
    }

    #[test]
    fn pending_turns_are_bounded() {
        let mut auto = AutoOutcomes::default();
        for i in 0..(MAX_PENDING as u32 + 50) {
            let id = Hash32::digest("m", &i.to_be_bytes());
            auto.after_turn("n", "q", &format!("answer {i}"), vec![id], 1);
        }
        assert_eq!(auto.waiting(), MAX_PENDING);
    }
}
