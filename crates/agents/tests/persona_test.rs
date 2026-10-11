//! Tests for `Persona::load_from_workspace` and `system_prompt` rendering.

use consistent_world_agents::persona::Persona;

#[test]
fn test_persona_load_from_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    std::fs::write(path.join("SOUL.md"), "You are helpful.").unwrap();
    std::fs::write(path.join("USER.md"), "The user is a developer.").unwrap();
    // AGENTS.md optional

    let persona = Persona::load_from_workspace(path).unwrap();
    assert!(persona.soul.as_deref() == Some("You are helpful."));
    assert!(persona.user_context.as_deref() == Some("The user is a developer."));
    assert!(persona.agent_instructions.is_none());

    let prompt = persona.system_prompt();
    assert!(prompt.contains("## SOUL"));
    assert!(prompt.contains("You are helpful."));
    assert!(prompt.contains("## USER"));
    assert!(prompt.contains("The user is a developer."));
}

#[test]
fn test_persona_empty_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let persona = Persona::load_from_workspace(dir.path()).unwrap();
    assert!(persona.soul.is_none());
    assert!(persona.user_context.is_none());
    assert!(persona.system_prompt().is_empty());
}
