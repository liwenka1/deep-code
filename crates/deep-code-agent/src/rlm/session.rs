use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::handle::HandleStore;
use crate::rlm::runtime::{AnalysisRuntime, DEFAULT_GREP_MAX_MATCHES, DEFAULT_MAX_INLINE_CHARS};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RlmConfig {
    pub max_inline_chars: usize,
    pub grep_max_matches: usize,
}

impl Default for RlmConfig {
    fn default() -> Self {
        Self {
            max_inline_chars: DEFAULT_MAX_INLINE_CHARS,
            grep_max_matches: DEFAULT_GREP_MAX_MATCHES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RlmSessionInfo {
    pub name: String,
    pub id: String,
    pub source_type: String,
    pub byte_len: usize,
    pub line_count: usize,
    pub eval_count: u32,
    pub config: RlmConfig,
    pub closed: bool,
}

struct RlmSession {
    info: RlmSessionInfo,
    runtime: AnalysisRuntime,
}

#[derive(Debug, thiserror::Error)]
pub enum RlmError {
    #[error("rlm session `{name}` already exists")]
    AlreadyExists { name: String },

    #[error("rlm session `{name}` not found")]
    NotFound { name: String },

    #[error("rlm session `{name}` is closed")]
    Closed { name: String },

    #[error("{message}")]
    InvalidInput { message: String },

    #[error("io error: {message}")]
    Io { message: String },
}

pub struct RlmManager {
    handle_store: Arc<RwLock<HandleStore>>,
    sessions: HashMap<String, RlmSession>,
}

impl RlmManager {
    #[must_use]
    pub fn new(handle_store: Arc<RwLock<HandleStore>>) -> Self {
        Self {
            handle_store,
            sessions: HashMap::new(),
        }
    }

    pub fn open(
        &mut self,
        name: String,
        context: String,
        source_type: impl Into<String>,
    ) -> Result<RlmSessionInfo, RlmError> {
        if self.sessions.contains_key(&name) {
            return Err(RlmError::AlreadyExists { name });
        }
        let byte_len = context.len();
        let line_count = context.lines().count();
        let info = RlmSessionInfo {
            name: name.clone(),
            id: format!("rlm_{}", new_suffix()),
            source_type: source_type.into(),
            byte_len,
            line_count,
            eval_count: 0,
            config: RlmConfig::default(),
            closed: false,
        };
        let runtime = AnalysisRuntime::new(context);
        self.sessions.insert(
            name,
            RlmSession {
                info: info.clone(),
                runtime,
            },
        );
        Ok(info)
    }

    pub fn configure(
        &mut self,
        name: &str,
        max_inline_chars: Option<usize>,
        grep_max_matches: Option<usize>,
    ) -> Result<RlmConfig, RlmError> {
        let session = self.session_mut(name)?;
        if let Some(max) = max_inline_chars {
            session.info.config.max_inline_chars = max.clamp(256, 50_000);
        }
        if let Some(max) = grep_max_matches {
            session.info.config.grep_max_matches = max.clamp(1, 10_000);
            session
                .runtime
                .set_grep_max_matches(session.info.config.grep_max_matches);
        }
        Ok(session.info.config.clone())
    }

    pub fn eval(
        &mut self,
        name: &str,
        code: &str,
    ) -> Result<crate::rlm::runtime::EvalOutput, RlmError> {
        let (session_name, config, raw) = {
            let session = self.session_mut(name)?;
            session.info.eval_count = session.info.eval_count.saturating_add(1);
            let config = session.info.config.clone();
            let session_name = session.info.name.clone();
            let raw = session
                .runtime
                .eval(code)
                .map_err(|message| RlmError::InvalidInput { message })?;
            (session_name, config, raw)
        };
        let mut store = self.handle_store.write().map_err(|error| RlmError::Io {
            message: error.to_string(),
        })?;
        Ok(crate::rlm::runtime::materialize_output(
            &mut store,
            &session_name,
            raw,
            config.max_inline_chars,
        ))
    }

    pub fn close(&mut self, name: &str) -> Result<RlmSessionInfo, RlmError> {
        let session = self
            .sessions
            .remove(name)
            .ok_or_else(|| RlmError::NotFound {
                name: name.to_string(),
            })?;
        let mut info = session.info;
        info.closed = true;
        if let Ok(mut store) = self.handle_store.write() {
            store.purge_session(&info.name);
        }
        Ok(info)
    }

    pub fn get(&self, name: &str) -> Result<RlmSessionInfo, RlmError> {
        Ok(self.session(name)?.info.clone())
    }

    pub fn list(&self) -> Vec<RlmSessionInfo> {
        self.sessions
            .values()
            .map(|session| session.info.clone())
            .collect()
    }

    pub fn close_all(&mut self) {
        let names: Vec<String> = self.sessions.keys().cloned().collect();
        for name in names {
            let _ = self.close(&name);
        }
    }

    fn session(&self, name: &str) -> Result<&RlmSession, RlmError> {
        self.sessions.get(name).ok_or_else(|| RlmError::NotFound {
            name: name.to_string(),
        })
    }

    fn session_mut(&mut self, name: &str) -> Result<&mut RlmSession, RlmError> {
        let session = self
            .sessions
            .get_mut(name)
            .ok_or_else(|| RlmError::NotFound {
                name: name.to_string(),
            })?;
        if session.info.closed {
            return Err(RlmError::Closed {
                name: name.to_string(),
            });
        }
        Ok(session)
    }
}

fn new_suffix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub fn derive_session_name(source_hint: Option<&str>) -> String {
    match source_hint {
        Some(hint) => {
            let path = PathBuf::from(hint);
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(hint);
            format!("rlm_{}", sanitize(stem))
        }
        None => format!("rlm_{}", new_suffix()),
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .take(32)
        .collect()
}
