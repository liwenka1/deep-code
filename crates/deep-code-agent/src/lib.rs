//! deep-code agent library (scaffold).

/// Placeholder for workspace wiring checks.
pub fn hello() -> &'static str {
    "hello from deep-code-agent"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_is_set() {
        assert_eq!(hello(), "hello from deep-code-agent");
    }
}
