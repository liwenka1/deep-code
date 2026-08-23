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
            "plan" | "planning" | "planner" => Ok(Self::Plan),
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
        // Only `implementer` may write, and (via `subagent_approval_decision`)
        // have those writes auto-approved. The default role is `general`, so a
        // bare `agent(task=...)` call with no role stays read-only — writing a
        // child that mutates the workspace unattended must be an explicit
        // `role: implementer` choice, not the silent default.
        matches!(self, Self::Implementer)
    }

    /// The model a child of this role is pinned to, or `None` to inherit the
    /// parent's configured model. Reconnaissance roles (explore / review /
    /// verifier) run the flash tier: their product is a report distilled from
    /// reads, and fan-out is exactly where per-child token spend multiplies —
    /// pinning them also skips per-turn auto-routing inside the child. Roles
    /// that write or plan (implementer / plan / general) inherit the parent's
    /// model: their output quality is the point of dispatching them.
    #[must_use]
    pub fn model_override(self) -> Option<&'static str> {
        match self {
            Self::Explore | Self::Review | Self::Verifier => {
                Some(crate::model_registry::DEEPSEEK_V4_FLASH)
            }
            Self::General | Self::Plan | Self::Implementer => None,
        }
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

pub fn build_system_prompt(role: SubAgentRole, network: bool) -> String {
    // Children run shell commands too and start from a blank context, so they
    // need the same host facts the parent gets — otherwise each one rediscovers
    // "this is not POSIX" one failed command at a time. The network line must
    // match the dispatch grant for the same reason the shell descriptions
    // match enforcement: a child that believes it is online when it is not
    // burns its step budget retrying doomed downloads.
    let network_block = if network {
        NETWORK_GRANTED_BLOCK
    } else {
        NETWORK_DENIED_BLOCK
    };
    format!(
        "{}\n{}\n\n{}\n\n{}",
        role.intro(),
        network_block,
        crate::extensions::platform_block(),
        super::output::SUBAGENT_OUTPUT_FORMAT
    )
}

const NETWORK_GRANTED_BLOCK: &str = "Network: this dispatch was approved WITH network access. \
You have fetch_url and web_search, and allow-listed sandboxed commands run with egress — no \
per-command network declaration is needed.";

const NETWORK_DENIED_BLOCK: &str = "Network: you have NO network access. Web tools are absent \
and network-declaring commands are auto-denied, so do not attempt downloads or remote calls. \
If the task genuinely needs the network, state that in your final report so the parent can \
re-dispatch with network=true.";

const GENERAL_INTRO: &str = "You are a general-purpose sub-agent spawned to handle a specific task autonomously.\nYou are read-only: investigate, search, and run read-only commands, but you cannot write files — if the task needs edits, report what should change under CHANGES and the parent can dispatch an `implementer`.\nStay inside the assigned scope; put adjacent work under RISKS/BLOCKERS.\n";

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
        assert_eq!(
            SubAgentRole::parse("explorer").unwrap(),
            SubAgentRole::Explore
        );
        assert_eq!(
            SubAgentRole::parse("code-review").unwrap(),
            SubAgentRole::Review
        );
        assert!(SubAgentRole::parse("custom").is_err());
    }

    /// Children run shell too and start blank, so they need the same host facts.
    #[test]
    fn child_prompt_states_the_host_shell() {
        let prompt = build_system_prompt(SubAgentRole::General, false);
        let expected_shell = if cfg!(windows) { "cmd.exe /C" } else { "sh -c" };
        assert!(
            prompt.contains(expected_shell),
            "child prompt must name the real shell: {prompt}"
        );
    }

    /// The prompt's network line must match the dispatch grant — a child that
    /// believes it is online when it is not burns its steps on doomed
    /// downloads, and one that believes it is offline never uses the web
    /// tools it was granted.
    #[test]
    fn child_prompt_states_the_real_network_grant() {
        let offline = build_system_prompt(SubAgentRole::Explore, false);
        assert!(
            offline.contains("NO network access"),
            "offline child must be told so: {offline}"
        );
        assert!(
            offline.contains("network=true"),
            "offline child must know the re-dispatch path: {offline}"
        );

        let online = build_system_prompt(SubAgentRole::Explore, true);
        assert!(
            online.contains("WITH network access"),
            "granted child must be told so: {online}"
        );
        assert!(
            online.contains("fetch_url"),
            "granted child must know its web tools: {online}"
        );
    }
}
