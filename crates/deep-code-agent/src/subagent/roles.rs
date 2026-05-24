use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::types::SubAgentError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentRole {
    General,
    Explore,
    Plan,
    Review,
    Implementer,
    Verifier,
}

impl SubAgentRole {
    pub fn parse(value: &str) -> Result<Self, SubAgentError> {
        match normalize(value).as_str() {
            "general" | "worker" | "default" | "general-purpose" => Ok(Self::General),
            "explore" | "explorer" | "exploration" => Ok(Self::Explore),
            "plan" | "planning" | "awaiter" => Ok(Self::Plan),
            "review" | "reviewer" | "code-review" | "code_review" => Ok(Self::Review),
            "implementer" | "implement" | "implementation" | "builder" => Ok(Self::Implementer),
            "verifier" | "verify" | "verification" | "validator" | "tester" => Ok(Self::Verifier),
            other => Err(SubAgentError::InvalidRole {
                value: other.to_string(),
            }),
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Explore => "explore",
            Self::Plan => "plan",
            Self::Review => "review",
            Self::Implementer => "implementer",
            Self::Verifier => "verifier",
        }
    }

    #[must_use]
    pub fn intro(self) -> &'static str {
        match self {
            Self::General => GENERAL_INTRO,
            Self::Explore => EXPLORE_INTRO,
            Self::Plan => PLAN_INTRO,
            Self::Review => REVIEW_INTRO,
            Self::Implementer => IMPLEMENTER_INTRO,
            Self::Verifier => VERIFIER_INTRO,
        }
    }

    #[must_use]
    pub fn allows_writes(self) -> bool {
        matches!(self, Self::General | Self::Implementer)
    }

    #[must_use]
    pub fn allows_shell(self) -> bool {
        !matches!(self, Self::Review)
    }
}

impl FromStr for SubAgentRole {
    type Err = SubAgentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

fn normalize(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

pub fn build_system_prompt(role: SubAgentRole) -> String {
    format!(
        "{}\n\n{}",
        role.intro(),
        super::output::SUBAGENT_OUTPUT_FORMAT
    )
}

const GENERAL_INTRO: &str = "You are a general-purpose sub-agent spawned to handle a specific task autonomously.\nStay inside the assigned scope; put adjacent work under RISKS/BLOCKERS.\n";

const EXPLORE_INTRO: &str = "You are an exploration sub-agent (role: `explore`). Map the relevant code quickly and stay read-only.\nUse list_dir, grep_files, and read_file; cite `path:line-range` for each finding.\nCHANGES will almost always be \"None.\" for an explorer.\n";

const PLAN_INTRO: &str = "You are a planning sub-agent. Produce a grounded, prioritized plan, not patches.\nRead enough code to avoid guessing; each step names its artifact and verification.\nCHANGES should list plan artifacts only, not speculative edits.\n";

const REVIEW_INTRO: &str = "You are a code review sub-agent. Stay read-only and report severity-scored findings.\nInclude path:line-range plus suggested fix. CHANGES will almost always be \"None.\" for a reviewer.\n";

const IMPLEMENTER_INTRO: &str = "You are an implementation sub-agent. Land the assigned change with minimal surrounding edits.\nRead target files before editing; run relevant verification after edit batches.\nCHANGES is load-bearing: list every modified file with a one-line why.\n";

const VERIFIER_INTRO: &str = "You are a verification sub-agent. Run requested gates and stay read-only.\nReport PASS/FAIL at the top of SUMMARY with exact command evidence.\nCHANGES will almost always be \"None.\" for a verifier.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_aliases_resolve() {
        assert_eq!(SubAgentRole::parse("explorer").unwrap(), SubAgentRole::Explore);
        assert_eq!(
            SubAgentRole::parse("code-review").unwrap(),
            SubAgentRole::Review
        );
        assert!(SubAgentRole::parse("custom").is_err());
    }
}
