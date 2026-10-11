//! Loads an agent's persona (SOUL/USER/AGENTS markdown files) from its
//! workspace directory and renders them into a single system prompt.

use std::path::Path;

/// The persona files loaded from an agent's workspace, if present.
pub struct Persona {
    /// Contents of SOUL.md: who the agent is.
    pub soul: Option<String>,
    /// Contents of USER.md: context about the human principal.
    pub user_context: Option<String>,
    /// Contents of AGENTS.md: operating instructions for the agent.
    pub agent_instructions: Option<String>,
}

impl Persona {
    /// Load SOUL.md, USER.md, AGENTS.md from workspace directory if present.
    pub fn load_from_workspace(workspace_path: &Path) -> anyhow::Result<Self> {
        // Missing file or read error both just mean "no persona section"; not fatal.
        let read_opt = |name: &str| {
            let p = workspace_path.join(name);
            if p.is_file() {
                std::fs::read_to_string(&p).ok()
            } else {
                None
            }
        };
        Ok(Self {
            soul: read_opt("SOUL.md"),
            user_context: read_opt("USER.md"),
            agent_instructions: read_opt("AGENTS.md"),
        })
    }

    /// Concatenate loaded files with section headers into a single system prompt.
    pub fn system_prompt(&self) -> String {
        let mut out = String::new();
        if let Some(ref s) = self.soul
            && !s.trim().is_empty()
        {
            out.push_str("## SOUL\n\n");
            out.push_str(s.trim());
            out.push_str("\n\n");
        }
        if let Some(ref s) = self.user_context
            && !s.trim().is_empty()
        {
            out.push_str("## USER\n\n");
            out.push_str(s.trim());
            out.push_str("\n\n");
        }
        if let Some(ref s) = self.agent_instructions
            && !s.trim().is_empty()
        {
            out.push_str("## AGENTS\n\n");
            out.push_str(s.trim());
            out.push_str("\n\n");
        }
        out.trim_end().to_string()
    }
}
