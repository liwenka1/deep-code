use std::collections::HashMap;

use regex::Regex;

use crate::handle::{HandleKind, HandleStore};

pub const DEFAULT_MAX_INLINE_CHARS: usize = 8_000;
pub const DEFAULT_GREP_MAX_MATCHES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalOutput {
    pub inline: String,
    pub stored_handle: bool,
    pub handle_id: Option<String>,
    pub line_count: usize,
    pub byte_len: usize,
}

#[derive(Debug, Clone)]
pub struct AnalysisRuntime {
    context: String,
    variables: HashMap<String, String>,
    grep_max_matches: usize,
}

impl AnalysisRuntime {
    #[must_use]
    pub fn new(context: String) -> Self {
        Self {
            context,
            variables: HashMap::new(),
            grep_max_matches: DEFAULT_GREP_MAX_MATCHES,
        }
    }

    pub fn set_grep_max_matches(&mut self, max: usize) {
        self.grep_max_matches = max.max(1);
    }

    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        let mut output = Vec::new();
        for line in code.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            output.push(self.eval_line(line)?);
        }
        if output.is_empty() {
            Ok("(no output)".to_string())
        } else {
            Ok(output.join("\n"))
        }
    }

    fn eval_line(&mut self, line: &str) -> Result<String, String> {
        let tokens = split_tokens(line);
        if tokens.is_empty() {
            return Ok(String::new());
        }
        match tokens[0].as_str() {
            "stats" | "context_stats" => {
                let lines = self.context.lines().count();
                Ok(format!(
                    "bytes={} lines={} chars={}",
                    self.context.len(),
                    lines,
                    self.context.chars().count()
                ))
            }
            "count_lines" => Ok(self.context.lines().count().to_string()),
            "head" => {
                let n = parse_usize(&tokens, 1, "head")?;
                Ok(take_lines(&self.context, n, true))
            }
            "tail" => {
                let n = parse_usize(&tokens, 1, "tail")?;
                Ok(take_lines(&self.context, n, false))
            }
            "lines" => {
                let start = parse_usize(&tokens, 1, "lines")?;
                let end = parse_usize(&tokens, 2, "lines")?;
                Ok(slice_lines(&self.context, start, end))
            }
            "grep" => {
                let pattern = rest_as_pattern(&tokens[1..])?;
                grep_lines(&self.context, &pattern, self.grep_max_matches)
            }
            "set" => {
                let name = tokens
                    .get(1)
                    .ok_or_else(|| "set requires a variable name".to_string())?
                    .clone();
                let value = if tokens.len() > 2 {
                    tokens[2..].join(" ")
                } else {
                    String::new()
                };
                self.variables.insert(name.clone(), value);
                Ok(format!("set {name}"))
            }
            "get" => {
                let name = tokens
                    .get(1)
                    .ok_or_else(|| "get requires a variable name".to_string())?;
                Ok(self
                    .variables
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| format!("(undefined {name})")))
            }
            "peek" => {
                let start = parse_usize(&tokens, 1, "peek")?;
                let end = tokens.get(2).and_then(|value| value.parse::<usize>().ok());
                let end = end.unwrap_or(start);
                Ok(slice_lines(&self.context, start, end))
            }
            other => Err(format!("unknown command '{other}'")),
        }
    }
}

pub fn materialize_output(
    store: &mut HandleStore,
    session_name: &str,
    text: String,
    max_inline: usize,
) -> EvalOutput {
    let line_count = text.lines().count();
    let byte_len = text.len();
    if text.chars().count() <= max_inline {
        return EvalOutput {
            inline: text,
            stored_handle: false,
            handle_id: None,
            line_count,
            byte_len,
        };
    }
    let summary = store.insert_text(
        format!("rlm:{session_name}"),
        HandleKind::RlmResult,
        text,
        Some(session_name.to_string()),
    );
    EvalOutput {
        inline: format!(
            "{{\"stored\":true,\"handle_id\":\"{}\",\"byte_len\":{byte_len},\"line_count\":{line_count}}}",
            summary.id.as_str()
        ),
        stored_handle: true,
        handle_id: Some(summary.id.as_str().to_string()),
        line_count,
        byte_len,
    }
}

fn split_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in line.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            ' ' | '\t' if !in_quote => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_usize(tokens: &[String], index: usize, command: &str) -> Result<usize, String> {
    tokens
        .get(index)
        .ok_or_else(|| format!("{command} requires argument {}", index))?
        .parse::<usize>()
        .map_err(|_| format!("{command} argument {} must be a positive integer", index))
}

fn rest_as_pattern(tokens: &[String]) -> Result<String, String> {
    if tokens.is_empty() {
        return Err("grep requires a pattern".to_string());
    }
    Ok(tokens.join(" "))
}

fn take_lines(text: &str, count: usize, from_head: bool) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if from_head {
        lines
            .into_iter()
            .take(count.max(1))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let start = lines.len().saturating_sub(count.max(1));
        lines[start..].join("\n")
    }
}

fn slice_lines(text: &str, start_line: usize, end_line: usize) -> String {
    if start_line == 0 {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = start_line.saturating_sub(1);
    let end = end_line.max(start_line).min(lines.len());
    if start >= lines.len() {
        return String::new();
    }
    lines[start..end].join("\n")
}

fn grep_lines(text: &str, pattern: &str, max_matches: usize) -> Result<String, String> {
    let regex = Regex::new(pattern).map_err(|error| format!("invalid grep pattern: {error}"))?;
    let mut matches = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if regex.is_match(line) {
            matches.push(format!("{}:{}", index + 1, line));
            if matches.len() >= max_matches {
                matches.push(format!("... truncated after {max_matches} matches"));
                break;
            }
        }
    }
    if matches.is_empty() {
        Ok("(no matches)".to_string())
    } else {
        Ok(matches.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_stats_and_grep() {
        let mut runtime = AnalysisRuntime::new("error one\nok two\nerror three\n".to_string());
        assert!(runtime.eval("stats").unwrap().contains("lines=3"));
        let grep = runtime.eval("grep error").unwrap();
        assert!(grep.contains("error one"));
    }
}
