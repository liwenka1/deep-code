//! Phase-2 Flash router: when the deterministic heuristic
//! ([`crate::auto_mode::classify_model`]) can't decide a turn, ask a cheap
//! `deepseek-v4-flash` thinking-off classifier — leveraging DeepSeek's cheap
//! Flash tier — to pick the model and thinking level from the recent context.
//! Bounded and best-effort: it only fires on the ambiguous gray zone, has a
//! hard timeout, and silently falls back to the heuristic on any failure.

use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::time::timeout;

use crate::auto_mode::{
    ModelClass, RouteContext, RouteSource, TurnRoute, classify_model, clamp_effort_to_model,
    resolve_turn_route,
};
use crate::client::LlmClient;
use crate::event::AgentEvent;
use crate::message::{Message, Role};
use crate::model::ChatRequest;
use crate::model_registry::{DEEPSEEK_V4_FLASH, DEEPSEEK_V4_PRO};
use crate::reasoning::ReasoningEffort;
use crate::runtime::AgentRuntime;

/// Per-message truncation for the context block.
const ROUTER_CONTEXT_CHARS: usize = 900;
/// Truncation for the latest prompt handed to the classifier.
const ROUTER_PROMPT_CHARS: usize = 4000;

impl<C: LlmClient + 'static> AgentRuntime<C> {
    /// Resolve a turn's route, escalating ambiguous turns to the Flash router.
    pub(super) async fn route_turn(&self, user_prompt: &str, ctx: RouteContext) -> TurnRoute {
        let heuristic =
            || resolve_turn_route(&self.config, &self.registry, user_prompt, self.is_subagent, ctx);

        // Only the auto + online + parent path consults the router.
        if !self.config.router_enabled
            || !self.config.auto_model_enabled()
            || self.is_subagent
            || self.client.provider_name() == "echo"
        {
            return heuristic();
        }
        if !matches!(
            classify_model(user_prompt, &ctx, self.config.auto_cost_saving),
            ModelClass::Ambiguous { .. }
        ) {
            return heuristic();
        }

        match self.flash_route(user_prompt).await {
            Some(route) => route,
            None => heuristic(),
        }
    }

    async fn flash_route(&self, user_prompt: &str) -> Option<TurnRoute> {
        let messages = self.router_messages(user_prompt).await;
        let mut request =
            ChatRequest::streaming(DEEPSEEK_V4_FLASH, messages).with_reasoning_effort("off");
        request.max_tokens = Some(32);

        let router_timeout = Duration::from_millis(self.config.router_timeout_ms);
        let text = timeout(router_timeout, collect_text(self.client.as_ref(), request))
            .await
            .ok()??;
        let decision = parse_router_decision(&text)?;
        Some(self.route_from_router(decision))
    }

    async fn router_messages(&self, user_prompt: &str) -> Vec<Message> {
        let context = {
            let state = self.state.lock().await;
            recent_context(
                state.session.messages(),
                self.config.router_context_turns,
                ROUTER_CONTEXT_CHARS,
            )
        };
        let prompt = truncate(user_prompt, ROUTER_PROMPT_CHARS);
        let user = if context.is_empty() {
            format!("Latest request:\n{prompt}")
        } else {
            format!("Recent context (oldest first):\n{context}\n\nLatest request:\n{prompt}")
        };
        vec![
            Message::system(router_system_prompt(self.config.auto_cost_saving)),
            Message::user(user),
        ]
    }

    fn route_from_router(&self, decision: RouterDecision) -> TurnRoute {
        let model = if decision.is_pro() {
            DEEPSEEK_V4_PRO
        } else {
            DEEPSEEK_V4_FLASH
        }
        .to_string();

        // Honor an explicit reasoning setting; only let the router pick the tier
        // when effort is on Auto.
        let auto_effort = self.config.reasoning_effort.is_auto();
        let effort = if auto_effort {
            decision.thinking_effort()
        } else {
            self.config.reasoning_effort.resolve(self.is_subagent, "")
        };
        let effective_effort = clamp_effort_to_model(&model, effort);

        let short_model = if model == DEEPSEEK_V4_PRO { "Pro" } else { "Flash" };
        TurnRoute {
            requested_model: self.config.model.clone(),
            effective_model: model,
            auto_model: true,
            reasoning_setting: self.config.reasoning_effort,
            effective_effort,
            auto_effort,
            used_model_fallback: false,
            route_reason: format!(
                "Flash 路由判定：中等难度任务交由 Flash 分类器，选择 {short_model}"
            ),
            fallback_reason: None,
            source: RouteSource::FlashRouter,
        }
    }
}

fn router_system_prompt(cost_saving: bool) -> String {
    let mut prompt = "You are deep-code's auto-routing classifier. Reply with ONLY compact JSON: \
{\"model\":\"flash|pro\",\"thinking\":\"off|low|high|max\"}. \
Use flash for trivial, conversational, status, or single-step work; \
use pro for coding, debugging, multi-step, multi-file, high-risk, tool-heavy, or ambiguous work that benefits from deeper reasoning. \
Use thinking off only for trivial no-tool answers, low for simple lookups, high for ordinary reasoning, and max for agentic/coding/debugging/architecture/security work."
        .to_string();
    if cost_saving {
        prompt.push_str(
            " Cost-saving mode is ON: resolve ambiguous cases in favour of flash, not pro.",
        );
    }
    prompt
}

async fn collect_text<C: LlmClient>(client: &C, request: ChatRequest) -> Option<String> {
    let mut stream = client.stream_chat(request).await.ok()?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(AgentEvent::TextDelta { text: delta }) => text.push_str(&delta),
            Ok(AgentEvent::Done { .. }) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct RouterDecision {
    model: String,
    #[serde(default)]
    thinking: Option<String>,
}

impl RouterDecision {
    fn is_pro(&self) -> bool {
        let model = self.model.trim().to_ascii_lowercase();
        model.contains("pro")
    }

    fn thinking_effort(&self) -> ReasoningEffort {
        match self.thinking.as_deref().map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("off") => ReasoningEffort::Off,
            Some("low") => ReasoningEffort::Low,
            Some("medium" | "med") => ReasoningEffort::Medium,
            Some("max") => ReasoningEffort::Max,
            _ => ReasoningEffort::High,
        }
    }
}

/// Extract and parse the first JSON object from the classifier's reply, which
/// may carry stray prose or code fences around it.
fn parse_router_decision(raw: &str) -> Option<RouterDecision> {
    let start = raw.find('{')?;
    let end = raw[start..].find('}')? + start;
    let json = &raw[start..=end];
    let decision: RouterDecision = serde_json::from_str(json).ok()?;
    let model = decision.model.trim().to_ascii_lowercase();
    // Reject hallucinated models so we fall back to the heuristic.
    if model.contains("pro") || model.contains("flash") {
        Some(decision)
    } else {
        None
    }
}

fn recent_context(messages: &[Message], turns: usize, max_chars: usize) -> String {
    let relevant: Vec<&Message> = messages
        .iter()
        .filter(|message| message.role != Role::System)
        .collect();
    // Drop the trailing current user prompt — it's sent separately.
    let end = relevant.len().saturating_sub(1);
    let start = end.saturating_sub(turns);
    relevant[start..end]
        .iter()
        .filter(|message| !message.content.trim().is_empty())
        .map(|message| {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                Role::System => "system",
            };
            format!("{role}: {}", truncate(&message.content, max_chars))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        let kept: String = value.chars().take(max_chars).collect();
        format!("{kept}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let decision = parse_router_decision(r#"{"model":"pro","thinking":"max"}"#).unwrap();
        assert!(decision.is_pro());
        assert_eq!(decision.thinking_effort(), ReasoningEffort::Max);
    }

    #[test]
    fn parses_json_with_surrounding_prose() {
        let decision =
            parse_router_decision("Here you go:\n```json\n{\"model\": \"flash\", \"thinking\": \"low\"}\n```")
                .unwrap();
        assert!(!decision.is_pro());
        assert_eq!(decision.thinking_effort(), ReasoningEffort::Low);
    }

    #[test]
    fn rejects_hallucinated_model() {
        assert!(parse_router_decision(r#"{"model":"gpt-9","thinking":"high"}"#).is_none());
    }

    #[test]
    fn missing_thinking_defaults_to_high() {
        let decision = parse_router_decision(r#"{"model":"pro"}"#).unwrap();
        assert_eq!(decision.thinking_effort(), ReasoningEffort::High);
    }

    #[test]
    fn recent_context_drops_current_prompt_and_system() {
        let messages = vec![
            Message::system("sys"),
            Message::user("first"),
            Message::assistant("reply"),
            Message::user("current prompt"),
        ];
        let context = recent_context(&messages, 6, 900);
        assert!(context.contains("user: first"));
        assert!(context.contains("assistant: reply"));
        assert!(!context.contains("current prompt"));
        assert!(!context.contains("sys"));
    }
}
