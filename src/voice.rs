//! Spoken prompts for guided training, so you can follow along with your
//! eyes off the screen. The text goes to a shell command on stdin and in
//! `$MUTIX_TEXT`; without one configured, `spd-say` reads it.

use std::io::Write;
use std::process::{Command, Stdio};

pub struct Voice {
    command: Option<String>,
    french: bool,
}

impl Voice {
    pub fn new(enabled: bool, command: Option<&str>) -> Voice {
        let lang = std::env::var("LANGUAGE")
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default();
        let french = lang.starts_with("fr");
        let command = enabled.then(|| {
            command
                .map(str::to_string)
                .unwrap_or_else(|| format!("spd-say -w -l {} \"$MUTIX_TEXT\"", if french { "fr" } else { "en" }))
        });
        Voice { command, french }
    }

    /// Speaks and waits until done; prints the text in any case.
    pub fn say(&self, text: &str) {
        eprintln!("{text}");
        let Some(cmd) = &self.command else { return };
        let child = Command::new("sh")
            .args(["-c", cmd])
            .env("MUTIX_TEXT", text)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn();
        match child {
            Ok(mut c) => {
                if let Some(mut stdin) = c.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                let _ = c.wait();
            }
            Err(e) => eprintln!("mutix: cannot run the speak command: {e}"),
        }
    }

    pub fn keep_still(&self) -> &'static str {
        if self.french {
            "Ne bouge plus, bouche fermée."
        } else {
            "Keep still, mouth closed."
        }
    }

    pub fn next_word(&self, word: &str, takes: usize) -> String {
        if self.french {
            format!("Mot suivant : {word}. Articule-le sans bruit chaque fois que je le dis. {takes} fois.")
        } else {
            format!("Next word: {word}. Mouth it silently each time I say it. {takes} times.")
        }
    }

    pub fn again(&self) -> &'static str {
        if self.french {
            "Je n'ai rien senti, encore."
        } else {
            "Nothing came through, again."
        }
    }

    pub fn done(&self) -> &'static str {
        if self.french {
            "Terminé, merci."
        } else {
            "Done, thank you."
        }
    }
}
