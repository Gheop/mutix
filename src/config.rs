//! `~/.config/mutix/config.toml`. Every key is optional.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::features::FeatureSet;
use crate::model::Params;

pub const SAMPLE: &str = r#"# mutix configuration. Every key is optional.

# AirPods to use, by address or name. Default: the first connected AirPods.
# device = "A4:C3:F0:12:34:56"

# Milliseconds between motion samples (5 to 40). 10 gives about 100 Hz.
# interval_ms = 10

# Sensor groups the recognizer compares: attitude, rotation, acceleration, gravity.
# `mutix eval --compare` shows which ones tell your words apart.
# features = ["attitude", "rotation", "acceleration"]

# Recognizer settings.
# band = 0.15           # time warping allowed, as a share of the word length
# max_distance = 4.0    # reject above this distance; default: learned from your takes
# min_margin = 0.08     # reject when two words are this close

# Auto trigger: a word starts when the motion exceeds this many times the
# noise, and ends once it falls below half of that.
# trigger = 3.0

# Keyboard layout used to type, e.g. "fr" or "us+dvorak". Default: GNOME's.
# keyboard = "fr"

# Command that speaks the training prompts; the text comes on stdin and in
# $MUTIX_TEXT. Default with `mutix train --speak`: spd-say.
# speak = "piper -m ~/piper/fr_FR-siwis-medium.onnx --output-raw | pacat --raw --rate=22050 --channels=1"

# Microphone used to notice when you speak aloud: then nothing is typed.
# Default: the default input. `pactl list short sources` lists the names.
# Avoid the AirPods' own microphone: it switches them to the low-quality
# headset profile.
# microphone = "alsa_input.pci-0000_00_1f.3.analog-stereo"
# voice_guard = true

# What each word does. Built-in words (oui, copie, entrée, yes, copy, enter…)
# have a default; any other word is typed followed by a space.
# [words.bonjour]
# text = "Bonjour !\n"
#
# [words.annule]
# keys = "ctrl+z"
#
# [words.musique]
# command = "playerctl play-pause"
"#;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub device: Option<String>,
    pub interval_ms: Option<u32>,
    pub features: Option<Vec<String>>,
    pub band: Option<f32>,
    pub max_distance: Option<f32>,
    pub min_margin: Option<f32>,
    pub trigger: Option<f32>,
    pub keyboard: Option<String>,
    pub speak: Option<String>,
    pub microphone: Option<String>,
    pub voice_guard: Option<bool>,
    pub words: BTreeMap<String, Action>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub text: Option<String>,
    /// Keys pressed together, xkb names joined by "+": "ctrl+c", "Return".
    pub keys: Option<String>,
    pub command: Option<String>,
}

pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mutix")
        .join("config.toml")
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("error in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// 10 ms by default. The AirPods accept 5 ms, but Bluetooth then delivers
    /// about 90 samples a second, no more than at 10 ms.
    pub fn interval_us(&self) -> u32 {
        self.interval_ms.unwrap_or(10).clamp(5, 40) * 1000
    }

    pub fn rate_hz(&self) -> f32 {
        1e6 / self.interval_us() as f32
    }

    pub fn trigger(&self) -> f32 {
        self.trigger.unwrap_or(3.0)
    }

    pub fn params(&self) -> Result<Params> {
        let d = Params::default();
        Ok(Params {
            set: match &self.features {
                Some(names) => FeatureSet::from_names(names)?,
                None => d.set,
            },
            band: self.band.unwrap_or(d.band),
            max_distance: self.max_distance.or(d.max_distance),
            min_margin: self.min_margin.unwrap_or(d.min_margin),
        })
    }

    /// What to do with a recognized word.
    /// The config entry, else the built-in vocabulary's, else the word typed
    /// followed by a space.
    pub fn action(&self, word: &str) -> Action {
        self.words
            .get(word)
            .cloned()
            .or_else(|| crate::vocab::default_action(word))
            .unwrap_or_else(|| Action {
                text: Some(format!("{word} ")),
                ..Action::default()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_parses_and_defaults_apply() {
        let c: Config = toml::from_str(SAMPLE).unwrap();
        assert_eq!(c.interval_us(), 10_000);
        assert_eq!(c.action("salut").text.as_deref(), Some("salut "));
        assert_eq!(c.action("copie").keys.as_deref(), Some("ctrl+c"));
        let c: Config = toml::from_str(
            "features = [\"rotation\"]\ninterval_ms = 20\n[words.suivant]\ncommand = \"playerctl next\"\n",
        )
        .unwrap();
        assert_eq!(c.params().unwrap().set.label(), "rotation");
        assert_eq!(c.rate_hz(), 50.0);
        assert_eq!(c.action("suivant").command.as_deref(), Some("playerctl next"));
        assert!(toml::from_str::<Config>("colour = 1").is_err());
    }
}
