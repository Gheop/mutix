//! Built-in vocabularies: words chosen to move the jaw and lips in clearly
//! different ways, each with a default action, so a trained word is useful
//! right away.

use crate::config::Action;

pub struct Entry {
    pub word: &'static str,
    /// Text typed, or keys pressed (xkb names joined by "+").
    pub text: Option<&'static str>,
    pub keys: Option<&'static str>,
}

pub struct Vocabulary {
    pub code: &'static str,
    pub name: &'static str,
    pub entries: &'static [Entry],
}

const fn text(word: &'static str, text: &'static str) -> Entry {
    Entry {
        word,
        text: Some(text),
        keys: None,
    }
}

const fn keys(word: &'static str, keys: &'static str) -> Entry {
    Entry {
        word,
        text: None,
        keys: Some(keys),
    }
}

pub const FRENCH: Vocabulary = Vocabulary {
    code: "fr",
    name: "Français",
    entries: &[
        text("oui", "oui "),
        text("non", "non "),
        text("bonjour", "Bonjour "),
        text("merci", "Merci "),
        keys("entrée", "Return"),
        keys("efface", "ctrl+BackSpace"),
        keys("annule", "ctrl+z"),
        keys("copie", "ctrl+c"),
        keys("colle", "ctrl+v"),
        keys("stop", "Escape"),
        keys("suivant", "Tab"),
        keys("espace", "space"),
    ],
};

pub const ENGLISH: Vocabulary = Vocabulary {
    code: "en",
    name: "English",
    entries: &[
        text("yes", "yes "),
        text("no", "no "),
        text("hello", "Hello "),
        text("thanks", "Thanks "),
        keys("enter", "Return"),
        keys("delete", "ctrl+BackSpace"),
        keys("undo", "ctrl+z"),
        keys("copy", "ctrl+c"),
        keys("paste", "ctrl+v"),
        keys("stop", "Escape"),
        keys("next", "Tab"),
        keys("space", "space"),
    ],
};

pub const ALL: [&Vocabulary; 2] = [&FRENCH, &ENGLISH];

impl Vocabulary {
    pub fn words(&self) -> Vec<&'static str> {
        self.entries.iter().map(|e| e.word).collect()
    }

    /// The vocabulary matching a locale such as "fr_FR.UTF-8"; English otherwise.
    pub fn for_locale(locale: &str) -> &'static Vocabulary {
        ALL.into_iter().find(|v| locale.starts_with(v.code)).unwrap_or(&ENGLISH)
    }
}

/// Default action of a built-in word, if it is one.
pub fn default_action(word: &str) -> Option<Action> {
    ALL.iter()
        .flat_map(|v| v.entries.iter())
        .find(|e| e.word == word)
        .map(|e| Action {
            text: e.text.map(str::to_string),
            keys: e.keys.map(str::to_string),
            command: None,
        })
}

/// Training lines: each word `reps` times, shuffled so the same word never
/// comes twice in a row, cut into lines of `per_line` words.
pub fn lines(words: &[&str], reps: usize, per_line: usize, seed: u64) -> Vec<Vec<String>> {
    let mut rng = seed | 1;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut pool: Vec<&str> = words.iter().flat_map(|w| std::iter::repeat_n(*w, reps)).collect();
    let mut order = Vec::with_capacity(pool.len());
    while !pool.is_empty() {
        let last = order.last().copied();
        let choices: Vec<usize> = (0..pool.len()).filter(|&i| Some(pool[i]) != last).collect();
        // Only one word left with several takes: accept the repeat.
        let i = if choices.is_empty() {
            0
        } else {
            choices[(next() % choices.len() as u64) as usize]
        };
        order.push(pool.swap_remove(i));
    }
    order
        .chunks(per_line.max(1))
        .map(|c| c.iter().map(|w| w.to_string()).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_balanced_without_repeats() {
        let words = FRENCH.words();
        let lines = lines(&words, 8, 4, 42);
        let flat: Vec<&String> = lines.iter().flatten().collect();
        assert_eq!(flat.len(), 96);
        assert_eq!(lines.len(), 24);
        for w in &words {
            assert_eq!(flat.iter().filter(|x| x.as_str() == *w).count(), 8);
        }
        assert!(flat.windows(2).all(|p| p[0] != p[1]));
    }

    #[test]
    fn defaults_and_locale() {
        assert_eq!(default_action("copie").and_then(|a| a.keys).as_deref(), Some("ctrl+c"));
        assert_eq!(default_action("hello").and_then(|a| a.text).as_deref(), Some("Hello "));
        assert!(default_action("salut").is_none());
        assert_eq!(Vocabulary::for_locale("fr_FR.UTF-8").code, "fr");
        assert_eq!(Vocabulary::for_locale("de_DE.UTF-8").code, "en");
    }
}
