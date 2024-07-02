pub mod serde_string {
    use std::str::FromStr;

    pub fn serialize<S>(value: &dyn std::fmt::Display, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        use serde::de::{Deserialize, Error};

        String::deserialize(deserializer).and_then(|s| T::from_str(&s).map_err(D::Error::custom))
    }
}
