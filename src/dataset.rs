//! Recorded takes, kept as raw frames so features can change later without
//! recording again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::frame::Frame;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Template {
    pub word: String,
    /// Unix time, seconds.
    pub recorded: u64,
    pub frames: Vec<Frame>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Dataset {
    pub version: u32,
    pub templates: Vec<Template>,
}

pub fn default_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mutix")
        .join("dataset.json")
}

impl Dataset {
    pub fn load(path: &Path) -> Result<Dataset> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("cannot read {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Dataset {
                version: 1,
                templates: Vec::new(),
            }),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Writes to a temporary file first so a crash never leaves half a dataset.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?).with_context(|| format!("cannot write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }

    pub fn add(&mut self, word: &str, frames: Vec<Frame>) {
        let recorded = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.templates.push(Template {
            word: word.to_string(),
            recorded,
            frames,
        });
    }

    /// Words with their number of takes, sorted by word.
    pub fn words(&self) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for t in &self.templates {
            *counts.entry(&t.word).or_default() += 1;
        }
        counts.into_iter().map(|(w, n)| (w.to_string(), n)).collect()
    }

    pub fn remove_last(&mut self, word: &str) -> bool {
        match self.templates.iter().rposition(|t| t.word == word) {
            Some(i) => {
                self.templates.remove(i);
                true
            }
            None => false,
        }
    }

    pub fn remove_word(&mut self, word: &str) -> usize {
        let before = self.templates.len();
        self.templates.retain(|t| t.word != word);
        before - self.templates.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_and_edit() {
        let dir = std::env::temp_dir().join(format!("mutix-test-{}", std::process::id()));
        let path = dir.join("dataset.json");
        let mut ds = Dataset::load(&path).unwrap();
        assert!(ds.templates.is_empty());
        ds.add("oui", Vec::new());
        ds.add("non", Vec::new());
        ds.add("oui", Vec::new());
        ds.save(&path).unwrap();
        let mut back = Dataset::load(&path).unwrap();
        assert_eq!(back.words(), vec![("non".to_string(), 1), ("oui".to_string(), 2)]);
        assert!(back.remove_last("oui"));
        assert_eq!(back.remove_word("non"), 1);
        assert_eq!(back.templates.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
