use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    #[must_use]
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }

    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_as_openai_compatible_lowercase() {
        let json = serde_json::to_string(&Message::user("hello")).unwrap();

        assert_eq!(json, r#"{"role":"user","content":"hello"}"#);
    }
}
