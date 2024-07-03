pub trait SendMessageExt {
    fn reply_to(self, message: &teloxide::prelude::Message) -> Self;

    fn markdown(self) -> Self;
}

impl SendMessageExt for teloxide::requests::JsonRequest<teloxide::payloads::SendMessage> {
    fn reply_to(mut self, message: &teloxide::prelude::Message) -> Self {
        self.reply_to_message_id = Some(message.id);
        self.message_thread_id = message.thread_id;
        self
    }

    fn markdown(mut self) -> Self {
        self.parse_mode = Some(teloxide::types::ParseMode::MarkdownV2);
        self.text = escape_markdown(std::mem::take(&mut self.text));
        self
    }
}

impl SendMessageExt for teloxide::requests::JsonRequest<teloxide::payloads::EditMessageText> {
    fn reply_to(self, _: &teloxide::prelude::Message) -> Self {
        self
    }

    fn markdown(mut self) -> Self {
        self.parse_mode = Some(teloxide::types::ParseMode::MarkdownV2);
        self.text = escape_markdown(std::mem::take(&mut self.text));
        self
    }
}

fn escape_markdown(text: impl Into<String>) -> String {
    static ESCAPED_CHARACTERS: [char; 17] = [
        '_', '*', '[', ']', '(', ')', '~', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
    ];

    static ESCAPED_CHARACTERS_REPLACEMENT: [&str; 17] = [
        "\\_", "\\*", "\\[", "\\]", "\\(", "\\)", "\\~", "\\>", "\\#", "\\+", "\\-", "\\=", "\\|",
        "\\{", "\\}", "\\.", "\\!",
    ];

    let mut text: String = text.into();
    for (character, replacement) in ESCAPED_CHARACTERS
        .iter()
        .zip(ESCAPED_CHARACTERS_REPLACEMENT.iter())
    {
        text = text.replace(*character, replacement);
    }
    text
}

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
