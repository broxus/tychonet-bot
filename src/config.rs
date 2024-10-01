use std::cmp::Ordering;
use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use similar::{ChangeTag, TextDiff};

pub struct Config {
    path: PathBuf,
    value: serde_json::Value,
    initial_value: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

pub fn parse_path(path: &str) -> Result<Vec<PathSegment>> {
    let mut segments = Vec::new();
    let mut current_segment = String::new();
    let mut in_brackets = false;

    for ch in path.chars() {
        match ch {
            '.' if !in_brackets => {
                if !current_segment.is_empty() {
                    segments.push(PathSegment::Key(current_segment.trim().to_string()));
                    current_segment.clear();
                }
            }
            '[' if !in_brackets => {
                if !current_segment.is_empty() {
                    segments.push(PathSegment::Key(current_segment.trim().to_string()));
                    current_segment.clear();
                }

                in_brackets = true;
            }
            ']' if in_brackets => {
                let trimmed = current_segment.trim();
                if trimmed.is_empty() {
                    return Err(anyhow!("Empty array index"));
                }
                let index = trimmed
                    .parse::<usize>()
                    .map_err(|_| anyhow!("Invalid array index: {}", trimmed))?;
                segments.push(PathSegment::Index(index));

                current_segment.clear();
                in_brackets = false;
            }
            _ => current_segment.push(ch),
        }
    }

    if in_brackets {
        return Err(anyhow!("Unclosed bracket"));
    }

    if !current_segment.is_empty() {
        segments.push(PathSegment::Key(current_segment.trim().to_string()));
    }

    // Check for empty keys eg ".key..key2"
    if segments
        .iter()
        .any(|seg| matches!(seg, PathSegment::Key(k) if k.is_empty()))
    {
        return Err(anyhow!("Empty keys are not allowed"));
    }

    Ok(segments)
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

    pub fn get(&self, path: &[PathSegment]) -> Result<&serde_json::Value> {
        let mut current = &self.value;
        let mut full_path = String::new();
        for segment in path {
            match segment {
                PathSegment::Key(key) => {
                    let serde_json::Value::Object(object) = current else {
                        return Err(container_expected(&full_path, ContainerType::Object));
                    };
                    full_path = format!("{full_path}.{key}");
                    current = object
                        .get(key)
                        .ok_or_else(|| anyhow::anyhow!("'{full_path}' not found"))?;
                }
                PathSegment::Index(index) => {
                    let serde_json::Value::Array(array) = current else {
                        return Err(container_expected(&full_path, ContainerType::Array));
                    };
                    full_path = format!("{full_path}[{index}]");
                    current = array
                        .get(*index)
                        .ok_or_else(|| anyhow::anyhow!("'{full_path}' not found"))?;
                }
            }
        }
        Ok(current)
    }

    pub fn set(&mut self, path: &[PathSegment], value: serde_json::Value) -> Result<()> {
        let mut current = &mut self.value;
        let mut full_path = String::new();
        for (i, segment) in path.iter().enumerate() {
            match segment {
                PathSegment::Key(key) => {
                    let serde_json::Value::Object(object) = current else {
                        return Err(container_expected(&full_path, ContainerType::Object));
                    };
                    full_path = format!("{full_path}.{key}");
                    // If this is the last segment, we need to insert the value into the object
                    if i == path.len() - 1 {
                        object.insert(key.clone(), value);
                        return Ok(());
                    }
                    current = object
                        .entry(key)
                        .or_insert_with(|| serde_json::Value::Object(Default::default()));
                }
                PathSegment::Index(index) => {
                    let serde_json::Value::Array(array) = current else {
                        return Err(container_expected(&full_path, ContainerType::Array));
                    };
                    full_path = format!("{full_path}[{index}]");

                    // If this is the last segment, we need to insert the value into the array
                    if i == path.len() - 1 {
                        match index.cmp(&array.len()) {
                            Ordering::Equal => {
                                array.push(value);
                            }
                            Ordering::Less => {
                                array[*index] = value;
                            }
                            Ordering::Greater => {
                                anyhow::bail!(
                                    "Index {index} out of bounds for array at '{full_path}'"
                                );
                            }
                        }

                        return Ok(());
                    }

                    #[allow(clippy::comparison_chain)]
                    if *index == array.len() {
                        array.push(serde_json::Value::Object(Default::default()));
                    } else if *index > array.len() {
                        anyhow::bail!("Index {index} out of bounds for array at '{full_path}'");
                    }
                    current = &mut array[*index];
                }
            }
        }
        Ok(())
    }

    pub fn remove(&mut self, path: &[PathSegment]) -> Result<()> {
        let mut current = &mut self.value;
        let mut full_path = String::new();
        let mut iter = path.iter().peekable();

        while let Some(segment) = iter.next() {
            match segment {
                PathSegment::Key(key) => {
                    let serde_json::Value::Object(object) = current else {
                        return Err(container_expected(&full_path, ContainerType::Object));
                    };
                    full_path = format!("{full_path}.{key}");
                    if iter.peek().is_none() {
                        object.remove(key);
                        return Ok(());
                    }
                    match object.get_mut(key) {
                        Some(value) => current = value,
                        None => return Ok(()),
                    }
                }
                PathSegment::Index(index) => {
                    let serde_json::Value::Array(array) = current else {
                        return Err(container_expected(&full_path, ContainerType::Array));
                    };
                    full_path = format!("{full_path}[{index}]");
                    if iter.peek().is_none() {
                        if *index < array.len() {
                            array.remove(*index);
                        }
                        return Ok(());
                    }
                    if *index >= array.len() {
                        return Ok(());
                    }
                    current = &mut array[*index];
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

            if let Some(lines) = lines_after_change {
                if lines > stack.len() {
                    write!(f, " ...")?;
                }
            }

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

enum ContainerType {
    Object,
    Array,
}

fn container_expected(path: &str, container_type: ContainerType) -> anyhow::Error {
    let path = if path.is_empty() { "." } else { path };
    let expected = match container_type {
        ContainerType::Object => "an object",
        ContainerType::Array => "an array",
    };
    anyhow::anyhow!("expected '{path}' to be {expected}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::NamedTempFile;

    fn create_test_config() -> (Config, NamedTempFile) {
        let config_json = json!({
            "server": {
                "host": "localhost",
                "port": 8080
            },
            "database": {
                "url": "postgres://user:pass@localhost/dbname",
                "max_connections": 100
            },
            "features": ["auth", "api", "websocket"],
            "nested_array": [
                ["a", "b"],
                ["c", "d"]
            ]
        });

        let file = NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            serde_json::to_string_pretty(&config_json).unwrap(),
        )
        .unwrap();

        let config = Config::from_file(file.path().to_str().unwrap()).unwrap();
        (config, file)
    }

    #[test]
    fn test_parse_path() {
        let cases = vec![
            (
                ".logger. outputs[1]",
                vec![
                    PathSegment::Key("logger".to_string()),
                    PathSegment::Key("outputs".to_string()),
                    PathSegment::Index(1),
                ],
            ),
            (
                "key1.key2[0].key3",
                vec![
                    PathSegment::Key("key1".to_string()),
                    PathSegment::Key("key2".to_string()),
                    PathSegment::Index(0),
                    PathSegment::Key("key3".to_string()),
                ],
            ),
            (
                "[0][ 1  ][2]",
                vec![
                    PathSegment::Index(0),
                    PathSegment::Index(1),
                    PathSegment::Index(2),
                ],
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(parse_path(input).unwrap(), expected);
        }
    }

    #[test]
    fn test_parse_path_errors() {
        assert!(parse_path("key[]").is_err());
        assert!(parse_path("key[").is_err());
        assert!(parse_path("key[a]").is_err());

        assert!(parse_path("key[1").is_err());
    }

    #[test]
    fn test_get_object_value() {
        let (config, _file) = create_test_config();
        let path = vec![
            PathSegment::Key("server".to_string()),
            PathSegment::Key("host".to_string()),
        ];
        let parsed_path = parse_path(".server.host").unwrap();
        assert_eq!(path, parsed_path);

        let value = config.get(&parsed_path).unwrap();
        assert_eq!(value, "localhost");
    }

    #[test]
    fn test_get_array_value() {
        let (config, _file) = create_test_config();
        let path = vec![
            PathSegment::Key("features".to_string()),
            PathSegment::Index(1),
        ];
        let parsed_path = parse_path(".features[1]").unwrap();
        assert_eq!(path, parsed_path);

        let value = config.get(&parsed_path).unwrap();
        assert_eq!(value, "api");
    }

    #[test]
    fn test_get_nested_array_value() {
        let (config, _file) = create_test_config();
        let path = vec![
            PathSegment::Key("nested_array".to_string()),
            PathSegment::Index(1),
            PathSegment::Index(0),
        ];
        let parsed_path = parse_path(".nested_array[1][0]").unwrap();
        assert_eq!(path, parsed_path);

        let value = config.get(&parsed_path).unwrap();
        assert_eq!(value, "c");
    }

    #[test]
    fn test_remove_works() {
        let (mut config, _file) = create_test_config();
        let path = vec![
            PathSegment::Key("server".to_string()),
            PathSegment::Key("host".to_string()),
        ];
        let parsed_path = parse_path(".server.host").unwrap();
        assert_eq!(path, parsed_path);

        config.remove(&parsed_path).unwrap();
        assert!(config.get(&parsed_path).is_err());

        let path = vec![
            PathSegment::Key("nested_array".to_string()),
            PathSegment::Index(1),
        ];
        let parsed_path = parse_path(".nested_array[1]").unwrap();
        assert_eq!(path, parsed_path);
        config.remove(&parsed_path).unwrap();

        assert!(config.get(&parsed_path).is_err());

        config.set(&parsed_path, json!(["e", "f"])).unwrap();
        let value = config.get(&parsed_path).unwrap();
        assert_eq!(value, &json!(["e", "f"]));

        let unexpected_path = vec![PathSegment::Key("bla".to_string()), PathSegment::Index(1)];
        let parsed_path = parse_path(".bla[1]").unwrap();
        assert_eq!(unexpected_path, parsed_path);

        assert!(config.remove(&parsed_path).is_ok()); //todo: is it ok to remove non-existing path?
        assert!(config.get(&parsed_path).is_err());
    }
}
