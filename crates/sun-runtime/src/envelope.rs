//! The common instruction envelope: one structure, rendered
//! per CLI from an editable markdown template.
//!
//! Templates use `{{task}}`, `{{constraints}}`, `{{gates}}`, `{{context}}` and
//! `{{handoff}}`. Built-in templates live in `templates/<adapter id>.md`; a user
//! overrides one by putting `<adapter id>.md` in their template directory
//! (`runtimes.template_dir` in sun.yaml). Every adapter's `start` sends
//! [`first_prompt`].

use std::path::Path;

use sun_core::{Error, Result, SessionSpec};

pub use sun_core::Envelope;

/// The built-in template for adapter `id`, if it ships one.
pub fn builtin(id: &str) -> Option<&'static str> {
    match id {
        "claude-code" => Some(include_str!("../templates/claude-code.md")),
        "codex" => Some(include_str!("../templates/codex.md")),
        "opencode" => Some(include_str!("../templates/opencode.md")),
        _ => None,
    }
}

/// The template for adapter `id`: `<user_dir>/<id>.md` when it exists,
/// otherwise the built-in one.
pub fn load(id: &str, user_dir: Option<&Path>) -> Result<String> {
    if let Some(path) = user_dir.map(|dir| dir.join(format!("{id}.md")))
        && path.is_file()
    {
        return std::fs::read_to_string(&path)
            .map_err(|e| Error::Unavailable(format!("template {}: {e}", path.display())));
    }
    builtin(id)
        .map(str::to_owned)
        .ok_or_else(|| Error::NotFound(format!("envelope template for `{id}`")))
}

/// The first prompt adapter `id` sends: `spec.envelope_parts` rendered into
/// its template (`template_dir` overrides the built-in), or `spec.envelope`
/// as is when the kernel sent no parts. A broken user template fails the
/// start rather than reaching the agent half-rendered.
pub fn first_prompt(id: &str, template_dir: Option<&Path>, spec: &SessionSpec) -> Result<String> {
    match &spec.envelope_parts {
        Some(parts) => render(&load(id, template_dir)?, parts),
        None => Ok(spec.envelope.clone()),
    }
}

/// Renders `envelope` into `template`. Lists become markdown bullets and empty
/// sections read "None.". Fails if the template drops `{{task}}` or still has
/// an unknown `{{placeholder}}` (a typo in a user-edited file).
pub fn render(template: &str, envelope: &Envelope) -> Result<String> {
    if !template.contains("{{task}}") {
        return Err(Error::InvalidInput(
            "envelope template has no {{task}} placeholder".into(),
        ));
    }
    // One pass over the template, so `{{…}}` inside the values stays literal.
    let mut out = String::with_capacity(template.len() + envelope.task.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let name = after.split("}}").next().unwrap_or_default();
        let value = match name {
            "task" => envelope.task.trim().to_owned(),
            "constraints" => bullets(&envelope.constraints),
            "gates" => bullets(&envelope.gates),
            "context" => bullets(&envelope.context),
            "handoff" => envelope
                .handoff
                .as_deref()
                .unwrap_or("None.")
                .trim()
                .to_owned(),
            _ => {
                return Err(Error::InvalidInput(format!(
                    "unknown envelope placeholder `{{{{{name}}}}}`"
                )));
            }
        };
        out.push_str(&value);
        rest = &after[name.len() + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        return "None.".into();
    }
    items
        .iter()
        .map(|item| format!("- {}", item.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_builtin_claude_template() {
        let envelope = Envelope {
            task: "Add a health endpoint.".into(),
            constraints: vec!["No new dependencies.".into()],
            gates: vec!["cargo test".into(), "cargo clippy".into()],
            context: vec![],
            handoff: None,
        };
        let out = render(&load("claude-code", None).unwrap(), &envelope).unwrap();
        assert!(out.contains("# Task\n\nAdd a health endpoint.\n"));
        assert!(out.contains("- cargo test\n- cargo clippy\n"));
        assert!(out.contains("## Context\n\nRead these when you need them:\n\nNone.\n"));
        assert!(!out.contains("{{"));
    }

    #[test]
    fn rejects_broken_user_templates() {
        let envelope = Envelope::default();
        assert!(render("no task here", &envelope).is_err());
        let err = render("{{task}} {{gate}}", &envelope).unwrap_err();
        assert!(err.to_string().contains("{{gate}}"), "{err}");
        let task = Envelope {
            task: "render {{name}} literally".into(),
            ..Envelope::default()
        };
        assert_eq!(
            render("{{task}}", &task).unwrap(),
            "render {{name}} literally"
        );
    }

    #[test]
    fn user_template_overrides_builtin() {
        let dir = std::env::temp_dir().join(format!("sun-envelope-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("claude-code.md"), "Do: {{task}}").unwrap();
        let template = load("claude-code", Some(&dir)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let envelope = Envelope {
            task: "x".into(),
            ..Envelope::default()
        };
        assert_eq!(render(&template, &envelope).unwrap(), "Do: x");
        assert!(load("unknown-cli", None).is_err());
    }
}
