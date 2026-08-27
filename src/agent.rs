//! The future AI-agent seam - deliberately disabled in v0.
//!
//! Filecraft reserves an `agent` command so a future opt-in assistant has a
//! stable place to live, but v0 ships no agent: nothing here invokes an
//! LLM, scans files, or changes anything. The only implementation is
//! [`DisabledAgent`], which explains the situation.
//!
//! The contract any future implementation must honor is documented in
//! `docs/agent-seam.md`: explicit file scope, dry-run preview, per-action
//! user approval, and an auditable action log. Code review should treat any
//! new [`Agent`] implementation as a security boundary change.

use std::path::PathBuf;

/// What the user asked the agent for, plus the context it would be allowed
/// to see. v0 constructs this but never acts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    /// Words after `agent` on the command line, verbatim.
    pub args: Vec<String>,
    /// Directory the user was in.
    pub cwd: PathBuf,
    /// The selected entry, if any - the *only* candidate file scope.
    pub selection: Option<PathBuf>,
}

/// A reply for the user. v0 replies are always explanatory text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReply {
    pub lines: Vec<String>,
}

/// The seam. Implementations decide nothing silently: a future real agent
/// must present a dry-run and obtain approval before any change.
pub trait Agent {
    /// Whether this agent can do anything at all.
    fn is_enabled(&self) -> bool;
    /// Handle a request. Must never mutate the filesystem in v0.
    fn handle(&self, request: &AgentRequest) -> AgentReply;
}

/// The only agent in v0: permanently off, explains why.
#[derive(Debug, Default, Clone, Copy)]
pub struct DisabledAgent;

impl Agent for DisabledAgent {
    fn is_enabled(&self) -> bool {
        false
    }

    fn handle(&self, request: &AgentRequest) -> AgentReply {
        let mut lines = vec![
            "The 'agent' command is not configured in this version.".to_string(),
            String::new(),
            "Filecraft v0 ships no AI agent: nothing is scanned, indexed,".to_string(),
            "or sent anywhere, and no autonomous changes are possible.".to_string(),
            String::new(),
            "A future opt-in agent will follow the contract in".to_string(),
            "docs/agent-seam.md:".to_string(),
            "  - explicit file scope (only files you name)".to_string(),
            "  - dry-run preview of every proposed change".to_string(),
            "  - your approval before anything is applied".to_string(),
            "  - an auditable log of every action".to_string(),
        ];
        if !request.args.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "(your request \"{}\" was not sent anywhere)",
                request.args.join(" ")
            ));
        }
        AgentReply { lines }
    }
}

/// The agent Filecraft wires in. v0: always the disabled one; there is no
/// configuration that enables anything else.
pub fn default_agent() -> DisabledAgent {
    DisabledAgent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(args: &[&str]) -> AgentRequest {
        AgentRequest {
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: PathBuf::from("/tmp"),
            selection: None,
        }
    }

    #[test]
    fn default_agent_is_disabled() {
        assert!(!default_agent().is_enabled());
    }

    #[test]
    fn disabled_agent_explains_not_configured() {
        let reply = DisabledAgent.handle(&request(&[]));
        let text = reply.lines.join("\n");
        assert!(text.contains("not configured"));
        assert!(text.contains("docs/agent-seam.md"));
        assert!(text.contains("dry-run"));
        assert!(text.contains("approval"));
    }

    #[test]
    fn disabled_agent_echoes_that_args_went_nowhere() {
        let reply = DisabledAgent.handle(&request(&["summarize", "notes.txt"]));
        let text = reply.lines.join("\n");
        assert!(text.contains("summarize notes.txt"));
        assert!(text.contains("not sent anywhere"));
    }
}
