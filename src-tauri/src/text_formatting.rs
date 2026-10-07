//! Deterministic formatting around optional AI post-processing.
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct PunctuationReplacement {
    pub phrase: String,
    pub replacement: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, specta::Type, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InitialCapitalization {
    #[default]
    Keep,
    Lower,
    Upper,
    AfterPeriod,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, specta::Type, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PeriodHandling {
    #[default]
    Keep,
    RemoveFinal,
    RemoveSentence,
    SpokenOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(default)]
pub struct TextFormatting {
    pub enabled: bool,
    pub spoken_punctuation: bool,
    pub initial_capitalization: InitialCapitalization,
    pub periods: PeriodHandling,
    pub replacements: Vec<PunctuationReplacement>,
}

impl Default for TextFormatting {
    fn default() -> Self {
        Self {
            enabled: true,
            spoken_punctuation: true,
            initial_capitalization: InitialCapitalization::AfterPeriod,
            periods: PeriodHandling::SpokenOnly,
            replacements: default_replacements(),
        }
    }
}

pub fn default_replacements() -> Vec<PunctuationReplacement> {
    [
        ("exclamation point", "!"),
        ("exclamation mark", "!"),
        ("question mark", "?"),
        ("full stop", "."),
        ("period", "."),
        ("comma", ","),
        ("semicolon", ";"),
        ("colon", ":"),
        ("new paragraph", "\n\n"),
        ("new line", "\n"),
    ]
    .into_iter()
    .map(|(phrase, replacement)| PunctuationReplacement {
        phrase: phrase.into(),
        replacement: replacement.into(),
    })
    .collect()
}

pub fn validate(config: &TextFormatting) -> Result<(), String> {
    if config.replacements.len() > 100 {
        return Err("Use at most 100 punctuation replacements".into());
    }
    let mut phrases = std::collections::HashSet::new();
    for entry in &config.replacements {
        let phrase = entry.phrase.trim();
        if phrase.is_empty() || phrase.len() > 200 || entry.replacement.len() > 100 {
            return Err(
                "Replacement phrases must contain 1–200 bytes; replacements at most 100 bytes"
                    .into(),
            );
        }
        if !phrases.insert(phrase.to_lowercase()) {
            return Err("Replacement phrases must be unique (ignoring case)".into());
        }
    }
    Ok(())
}

/// Replace phrases in one pass, so replacements never trigger other rules.
/// A spoken period is kept distinguishable until final formatting, preserving explicit commands.
pub fn replace_spoken(text: &str, config: &TextFormatting) -> String {
    if !config.enabled || !config.spoken_punctuation {
        return text.into();
    }
    let mut entries: Vec<_> = config
        .replacements
        .iter()
        .filter(|e| !e.phrase.trim().is_empty())
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.phrase.len()));
    if entries.is_empty() {
        return text.into();
    }
    let pattern = format!(
        r"(?i)(?:{})",
        entries
            .iter()
            .map(|e| regex::escape(e.phrase.trim()))
            .collect::<Vec<_>>()
            .join("|")
    );
    let Ok(regex) = Regex::new(&pattern) else {
        return text.into();
    };
    let mut result = String::new();
    let mut cursor = 0;
    for found in regex.find_iter(text) {
        if found.start() < cursor {
            continue;
        }
        let word = |c: char| c.is_alphanumeric() || c == '_';
        if text[..found.start()].chars().next_back().is_some_and(word)
            || text[found.end()..].chars().next().is_some_and(word)
        {
            continue;
        }
        let Some(entry) = entries
            .iter()
            .find(|e| e.phrase.trim().to_lowercase() == found.as_str().to_lowercase())
        else {
            continue;
        };
        result.push_str(&text[cursor..found.start()]);
        let punctuation = matches!(
            entry.replacement.as_str(),
            "." | "," | ";" | ":" | "!" | "?"
        );
        if punctuation {
            while result.ends_with([' ', '\t', ',', '.', ';', ':']) {
                result.pop();
            }
        }
        if entry.replacement.contains('\n') {
            while result.ends_with([' ', '\t']) {
                result.pop();
            }
        }
        if entry.replacement == "." {
            result.push('\u{e000}');
        } else {
            result.push_str(&entry.replacement);
        }
        cursor = found.end();
        if entry.replacement.contains('\n') {
            while text[cursor..].starts_with([' ', '\t']) {
                cursor += 1;
            }
        }
        if punctuation {
            // Discard a recognizer-added period directly after a spoken command.
            if text[cursor..].starts_with('.') {
                cursor += 1;
            }
        }
    }
    result.push_str(&text[cursor..]);
    result
}

/// Format a fragment without editor context. Context-aware dictation should call
/// `finish_with_context`; this helper treats the fragment as a continuation.
pub fn finish(text: &str, config: &TextFormatting) -> String {
    finish_with_context(text, config, Some(false))
}

/// Format a dictation using the insertion context supplied by the caller.
/// Unknown context starts with a capital; successful-output fallback belongs to
/// the paste pipeline, so previews and failed/cancelled pastes have no side effects.
pub fn finish_with_context(
    text: &str,
    config: &TextFormatting,
    capitalize: Option<bool>,
) -> String {
    if !config.enabled {
        return text.into();
    }
    let replaced = replace_spoken(text, config);
    let chars: Vec<_> = replaced.chars().collect();
    let final_index = chars
        .iter()
        .rposition(|c| !c.is_whitespace() && !is_closing_punctuation(*c));
    let mut pending = Some(capitalize.unwrap_or(true));
    let mut result = String::new();

    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|p| chars.get(p)).copied();
        let next = chars.get(i + 1).copied();
        let decimal =
            prev.is_some_and(|v| v.is_ascii_digit()) && next.is_some_and(|v| v.is_ascii_digit());
        let ellipsis = prev == Some('.') || next == Some('.');
        let sentence_period = c == '.'
            && !decimal
            && next.is_none_or(|v| v.is_whitespace() || is_closing_punctuation(v));
        let remove = c == '.'
            && !decimal
            && (!ellipsis || config.periods == PeriodHandling::SpokenOnly)
            && match config.periods {
                PeriodHandling::Keep => false,
                PeriodHandling::RemoveFinal => Some(i) == final_index,
                PeriodHandling::RemoveSentence => sentence_period,
                PeriodHandling::SpokenOnly => true,
            };

        if config.initial_capitalization == InitialCapitalization::AfterPeriod {
            if matches!(c, '\u{e000}' | '?' | '!' | '\n' | '\r') {
                // A boundary dictated within this fragment takes precedence over
                // the editor's preceding unfinished sentence.
                pending = Some(true);
            } else if sentence_period {
                if !remove {
                    pending = Some(true);
                } else if pending != Some(true) {
                    // Remove the recognizer's sentence capitalization together
                    // with its automatic period, but preserve an explicit boundary.
                    pending = Some(false);
                }
            }
            if c.is_alphabetic() {
                if let Some(upper) = pending.take() {
                    if upper {
                        result.extend(c.to_uppercase());
                    } else {
                        result.extend(c.to_lowercase());
                    }
                    continue;
                }
            }
        }
        if !remove {
            result.push(if c == '\u{e000}' { '.' } else { c });
        }
    }

    if matches!(
        config.initial_capitalization,
        InitialCapitalization::Lower | InitialCapitalization::Upper
    ) {
        if let Some((index, first)) = result.char_indices().find(|(_, c)| c.is_alphabetic()) {
            let replacement: String = match config.initial_capitalization {
                InitialCapitalization::Lower => first.to_lowercase().collect(),
                InitialCapitalization::Upper => first.to_uppercase().collect(),
                _ => unreachable!(),
            };
            result.replace_range(index..index + first.len_utf8(), &replacement);
        }
    }
    result
}

fn is_closing_punctuation(c: char) -> bool {
    matches!(c, '\"' | '\'' | '”' | '’' | ')' | ']' | '}' | '»' | '›')
}

/// Whether successfully inserted text leaves the caret at a new sentence/line.
/// Horizontal trailing whitespace preserves the boundary, including after a
/// newline-only dictation. Empty/space-only fragments do not create a boundary.
pub fn ends_at_sentence_boundary(text: &str) -> bool {
    let tail = text.trim_end_matches(|c: char| c.is_whitespace() && c != '\n' && c != '\r');
    if tail.ends_with(['\n', '\r']) {
        return true;
    }
    tail.trim_end_matches(is_closing_punctuation)
        .ends_with(['.', '?', '!'])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> TextFormatting {
        TextFormatting {
            enabled: true,
            spoken_punctuation: true,
            replacements: default_replacements(),
            initial_capitalization: InitialCapitalization::Keep,
            periods: PeriodHandling::Keep,
            ..Default::default()
        }
    }
    #[test]
    fn insertion_context_controls_only_the_initial_sentence() {
        let c = TextFormatting::default();
        assert_eq!(finish_with_context("Hello world.", &c, None), "Hello world");
        assert_eq!(
            finish_with_context("Hello world.", &c, Some(true)),
            "Hello world"
        );
        assert_eq!(
            finish_with_context("Hello world.", &c, Some(false)),
            "hello world"
        );
        assert_eq!(
            finish_with_context("World period Next", &c, Some(false)),
            "world. Next"
        );
        assert_eq!(
            finish_with_context("new line hello", &c, Some(false)),
            "\nHello"
        );
        assert_eq!(
            finish_with_context("period hello", &c, Some(false)),
            ". Hello"
        );
        assert_eq!(
            finish_with_context("question mark hello", &c, Some(false)),
            "? Hello"
        );
        assert_eq!(
            finish_with_context("exclamation point hello", &c, Some(false)),
            "! Hello"
        );
        // Formatting another editor or running a preview cannot alter any decision.
        assert_eq!(
            finish_with_context("Unrelated sentence", &c, Some(true)),
            "Unrelated sentence"
        );
        assert_eq!(
            finish_with_context("Still continuing", &c, Some(false)),
            "still continuing"
        );
        assert_eq!(
            finish_with_context("Unknown editor", &c, None),
            "Unknown editor"
        );
    }

    #[test]
    fn boundaries_include_punctuation_only_and_indented_newlines() {
        for text in [
            ".",
            "?",
            "!",
            "Hello.  ",
            "Hello?\" ",
            "Hello!')  ",
            "\n",
            "\r\n\t ",
            "hello\n  ",
        ] {
            assert!(
                ends_at_sentence_boundary(text),
                "expected boundary: {text:?}"
            );
        }
        for text in [
            "",
            "  ",
            "hello",
            "hello ",
            "hello, ",
            "hello;",
            "hello\ncontinued",
            "3.14",
        ] {
            assert!(
                !ends_at_sentence_boundary(text),
                "unexpected boundary: {text:?}"
            );
        }
        let c = TextFormatting::default();
        for command in [
            "new line",
            "new paragraph",
            "period",
            "question mark",
            "exclamation point",
        ] {
            let output = finish_with_context(command, &c, Some(false));
            assert!(
                ends_at_sentence_boundary(&output),
                "command: {command:?}, output: {output:?}"
            );
        }
    }

    #[test]
    fn capitalization_follows_the_periods_that_are_retained() {
        let mut c = TextFormatting::default();
        c.periods = PeriodHandling::Keep;
        assert_eq!(
            finish_with_context("hello. next", &c, Some(true)),
            "Hello. Next"
        );
        assert_eq!(
            finish_with_context("hello. next", &c, Some(false)),
            "hello. Next"
        );
        assert_eq!(
            finish_with_context("hello? next! again", &c, Some(false)),
            "hello? Next! Again"
        );
        assert_eq!(
            finish_with_context("value 3.14 meters. next", &c, Some(false)),
            "value 3.14 meters. Next"
        );
        assert_eq!(
            finish_with_context("visit example.com today. next", &c, Some(false)),
            "visit example.com today. Next"
        );
        c.periods = PeriodHandling::RemoveFinal;
        assert_eq!(
            finish_with_context("hello. next.", &c, Some(true)),
            "Hello. Next"
        );
        c.periods = PeriodHandling::SpokenOnly;
        assert_eq!(
            finish_with_context("Hello. Next period Again.", &c, Some(true)),
            "Hello next. Again"
        );
    }

    #[test]
    fn defaults_use_only_spoken_periods_and_capitalize_after_them() {
        assert_eq!(
            finish("Hello exclamation point.", &TextFormatting::default()),
            "hello!"
        );
        let c = TextFormatting::default();
        assert_eq!(
            finish("Hello. World period. Next sentence.", &c),
            "hello world. Next sentence"
        );
        assert_eq!(finish("Hello? World! Again.", &c), "hello? World! Again");
        assert_eq!(finish("Version 3.14 period Next", &c), "version 3.14. Next");
        assert_eq!(finish("Wait... Again.", &c), "wait again");
        assert_eq!(
            finish("Hello period . next sentence.", &c),
            "hello.  Next sentence"
        );
        assert_eq!(finish("Hello new line next line.", &c), "hello\nNext line");
        assert_eq!(finish_with_context("Hello.", &c, Some(true)), "Hello");
        assert_eq!(
            finish_with_context("Hello period", &c, Some(true)),
            "Hello."
        );
        assert_eq!(
            finish_with_context("Next word.", &c, Some(false)),
            "next word"
        );
    }
    #[test]
    fn spoken_commands_and_word_boundaries() {
        let c = config();
        assert_eq!(finish("Hello, EXCLAMATION POINT.", &c), "Hello!");
        assert_eq!(finish("One comma two question mark.", &c), "One, two?");
        assert_eq!(finish("Periodic table.", &c), "Periodic table.");
        assert_eq!(finish("One new paragraph two", &c), "One\n\ntwo");
    }
    #[test]
    fn independent_options_and_explicit_periods() {
        let mut c = config();
        c.initial_capitalization = InitialCapitalization::Lower;
        c.periods = PeriodHandling::RemoveFinal;
        assert_eq!(finish("Hello. World. ", &c), "hello. World ");
        assert_eq!(finish("Hello period.", &c), "hello.");
        assert_eq!(finish("Really?", &c), "really?");
        assert_eq!(finish("3.14...", &c), "3.14...");
        c.periods = PeriodHandling::RemoveSentence;
        assert_eq!(finish("Hello. World.", &c), "hello World");
    }
    #[test]
    fn unicode_and_internal_names_keep_their_case() {
        let c = TextFormatting::default();
        assert_eq!(
            finish_with_context(
                "Continue with NASA, iPhone, McDonald and Élan.",
                &c,
                Some(false)
            ),
            "continue with NASA, iPhone, McDonald and Élan"
        );
        assert_eq!(
            finish_with_context("\"Élan\" and Örebro", &c, Some(false)),
            "\"élan\" and Örebro"
        );
        assert_eq!(
            finish_with_context("éclair period über", &c, Some(true)),
            "Éclair. Über"
        );
        assert_eq!(finish_with_context("ßeta", &c, Some(true)), "SSeta");
        assert_eq!(finish_with_context("😀 élève", &c, Some(true)), "😀 Élève");
        assert_eq!(finish_with_context("", &c, None), "");
    }

    #[test]
    fn explicit_case_preferences_ignore_context_and_newlines_survive() {
        let mut c = TextFormatting::default();
        c.initial_capitalization = InitialCapitalization::Lower;
        assert_eq!(finish_with_context("\"Élan.\"", &c, Some(true)), "\"élan\"");
        c.initial_capitalization = InitialCapitalization::Upper;
        assert_eq!(finish_with_context("élan", &c, Some(false)), "Élan");
        c.initial_capitalization = InitialCapitalization::Keep;
        assert_eq!(finish_with_context("eBay", &c, Some(true)), "eBay");
        c = TextFormatting::default();
        assert_eq!(finish_with_context("new line.", &c, Some(false)), "\n");
        assert_eq!(
            finish_with_context("new paragraph hello", &c, Some(false)),
            "\n\nHello"
        );
        assert_eq!(
            finish_with_context("Hello.\nNext.", &c, Some(false)),
            "hello\nNext"
        );
        c.periods = PeriodHandling::RemoveFinal;
        assert_eq!(
            finish_with_context("hello. \"next.\" ", &c, Some(true)),
            "Hello. \"Next\" "
        );
        c.enabled = false;
        assert_eq!(
            finish_with_context("Hello period. Next.", &c, Some(false)),
            "Hello period. Next."
        );
    }
    #[test]
    fn rules_are_literal_non_recursive_and_validated() {
        let mut c = config();
        c.replacements = vec![PunctuationReplacement {
            phrase: "a+b".into(),
            replacement: "$1 comma".into(),
        }];
        assert_eq!(finish("a+b", &c), "$1 comma");
        c.replacements.push(PunctuationReplacement {
            phrase: "A+B".into(),
            replacement: "!".into(),
        });
        assert!(validate(&c).is_err());
    }
}
