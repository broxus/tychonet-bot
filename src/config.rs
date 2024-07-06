use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Context, Result};
use similar::{Change, ChangeTag, TextDiff};

pub struct Config {
    path: PathBuf,
    value: serde_json::Value,
    initial_value: String,
}

impl Config {
    pub fn from_file(path: &str) -> Result<Self> {
        let config_str = std::fs::read_to_string(path).context("Failed to read config file")?;
        let value = serde_json::from_str(&config_str).context("Failed to parse config file")?;
        Ok(Self {
            path: PathBuf::from(path),
            value,
            initial_value: config_str,
        })
    }

    pub fn save(self) -> Result<ConfigDiff> {
        let new_value =
            serde_json::to_string_pretty(&self.value).context("Failed to serialize config")?;
        std::fs::write(&self.path, &new_value).context("Failed to write config file")?;

        Ok(ConfigDiff {
            old: self.initial_value,
            new: new_value,
        })
    }

    pub fn get(&self, path: &[String]) -> Result<&serde_json::Value> {
        let mut current = &self.value;
        let mut full_path = String::new();
        for key in path {
            let serde_json::Value::Object(object) = current else {
                return Err(object_expected(&full_path));
            };
            full_path = format!("{full_path}.{key}");

            match object.get(key) {
                Some(value) => current = value,
                None => anyhow::bail!("'{full_path}' not found"),
            }
        }

        Ok(current)
    }

    pub fn set(&mut self, path: &[String], value: serde_json::Value) -> Result<()> {
        let mut current = &mut self.value;
        let mut full_path = String::new();
        for key in path {
            let serde_json::Value::Object(object) = current else {
                return Err(object_expected(&full_path));
            };
            full_path = format!("{full_path}.{key}");

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
            let serde_json::Value::Object(object) = current else {
                return Err(object_expected(&full_path));
            };
            full_path = format!("{full_path}.{key}");

            match object.entry(key) {
                serde_json::map::Entry::Occupied(entry) if iter.peek().is_none() => {
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

pub struct ConfigDiff {
    old: String,
    new: String,
}

impl std::fmt::Display for ConfigDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const LINES_BEFORE: usize = 3;
        const LINES_AFTER: usize = 3;

        let diff = TextDiff::from_lines(&self.old, &self.new);

        let mut stack = VecDeque::with_capacity(LINES_BEFORE);
        let mut lines_after_change = None::<usize>;
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => {
                    if let Some(lines) = &mut lines_after_change {
                        *lines += 1;

                        if *lines <= LINES_AFTER {
                            write!(f, " {change}")?;
                            continue;
                        }
                    }

                    if stack.len() >= LINES_BEFORE {
                        stack.pop_front();
                    }
                    stack.push_back(change);
                    continue;
                }
            };

            while let Some(change) = stack.pop_front() {
                write!(f, " {}", change)?;
            }
            write!(f, "{sign}{change}")?;

            lines_after_change = Some(0);
        }

        if lines_after_change.is_none() {
            write!(f, "unchanged")?;
        }

        Ok(())
    }
}

fn object_expected(path: &str) -> anyhow::Error {
    let path = if path.is_empty() { "." } else { path };
    anyhow::anyhow!("expected '{path}' to be an object")
}
