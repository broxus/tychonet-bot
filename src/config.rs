use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct Config {
    path: PathBuf,
    value: serde_json::Value,
}

impl Config {
    pub fn from_file(path: &str) -> Result<Self> {
        let config_str = std::fs::read_to_string(path).context("Failed to read config file")?;
        let value = serde_json::from_str(&config_str).context("Failed to parse config file")?;
        Ok(Self {
            path: PathBuf::from(path),
            value,
        })
    }

    pub fn save(&self) -> Result<()> {
        let data =
            serde_json::to_string_pretty(&self.value).context("Failed to serialize config")?;
        std::fs::write(&self.path, data).context("Failed to write config file")
    }

    pub fn set(&mut self, path: &[String], value: serde_json::Value) -> Result<()> {
        let mut current = &mut self.value;
        let mut full_path = String::new();
        for key in path {
            full_path = format!("{full_path}{key}");
            let serde_json::Value::Object(object) = current else {
                anyhow::bail!(
                    "expected '{}' to be an object",
                    full_path.trim_end_matches(|c| c != '.')
                );
            };

            current = object
                .entry(key)
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
        }

        *current = value;
        Ok(())
    }

    pub fn remove(&mut self, path: &[String]) -> Result<()> {
        let mut current = &mut self.value;

        let mut full_path = String::new();
        let mut iter = path.iter().peekable();

        while let Some(key) = iter.next() {
            full_path = format!("{full_path}.{key}");
            let serde_json::Value::Object(object) = current else {
                anyhow::bail!(
                    "expected '{}' to be an object",
                    full_path.trim_end_matches(|c| c != '.')
                );
            };

            match object.entry(key) {
                serde_json::map::Entry::Occupied(entry) if iter.peek().is_some() => {
                    entry.remove();
                    return Ok(());
                }
                serde_json::map::Entry::Occupied(entry) => {
                    current = entry.into_mut();
                }
                serde_json::map::Entry::Vacant(_) => {
                    return Ok(());
                }
            }
        }

        Ok(())
    }
}
