//! Which workspace repos a workflow run works in, read from the text of the
//! task and the workflow: when the run is queued (`queue::queue_run`) and
//! when a task is leased before it ran (`market`). [`run_repos`] is the one
//! owner of the rule; [`resolve_repos`] is the pure matcher.

use std::collections::HashSet;

use sun_core::model::{Repo, Task};
use sun_core::{Error, RepoId, Result, WorkflowId};
use sun_store::{AgentRepo, RepoRepo, Tx};

/// The repos a run of `task` on `workflow` works in: none on an agent's
/// system workflow (it clones for itself), else the workspace repos the
/// task's title and body and the workflow's name and `description` name.
/// `InvalidInput` when a coding workflow ([`sun_workflow::Workflow::is_coding`])
/// names none while the workspace has repos to name.
pub(crate) async fn run_repos(
    tx: &mut Tx,
    task: &Task,
    workflow_id: WorkflowId,
    workflow: &sun_workflow::Workflow,
) -> Result<Vec<RepoId>> {
    if tx.system_agent_of(workflow_id).await?.is_some() {
        return Ok(Vec::new());
    }
    let repos = tx.repos(task.workspace_id).await?;
    let text = format!(
        "{}\n{}\n{}\n{}",
        task.title, task.body, workflow.name, workflow.description
    );
    let found = resolve_repos(&text, &repos);
    if found.is_empty() && workflow.is_coding() && !repos.is_empty() {
        return Err(Error::InvalidInput(
            "No repository mentioned; add one to the task or workflow".into(),
        ));
    }
    Ok(found)
}

/// The repos `text` names, in `repos` order. A repo matches when its name
/// (the URL's last segment without `.git`) split into lowercase alphanumeric
/// tokens appears as a contiguous run of the text's tokens (`vps-infra` in
/// "Harden VPS Infra firewall", "vps_infra"), or joined as one token of the
/// text ("VPSINFRA"); whole tokens only, so `app` never matches "happy".
/// Or when every word of three or more characters of its description is a
/// word of the text.
pub(crate) fn resolve_repos(text: &str, repos: &[Repo]) -> Vec<RepoId> {
    let tokens: Vec<String> = tokens_of(text).collect();
    let words: HashSet<&str> = tokens
        .iter()
        .filter(|t| t.chars().count() >= 3)
        .map(String::as_str)
        .collect();
    repos
        .iter()
        .filter(|repo| {
            let name: Vec<String> = tokens_of(repo_name(&repo.url)).collect();
            let joined = name.concat();
            let named = !name.is_empty()
                && (tokens.windows(name.len()).any(|run| run == name.as_slice())
                    || tokens.contains(&joined));
            let mut described = words_of(&repo.description).peekable();
            named || (described.peek().is_some() && described.all(|w| words.contains(w.as_str())))
        })
        .map(|repo| repo.id)
        .collect()
}

/// The lowercase alphanumeric tokens of `text`.
fn tokens_of(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
}

/// The tokens of `text` with three or more characters.
fn words_of(text: &str) -> impl Iterator<Item = String> + '_ {
    tokens_of(text).filter(|w| w.chars().count() >= 3)
}

/// The last path segment of a clone URL, without `.git`.
fn repo_name(url: &str) -> &str {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default();
    last.strip_suffix(".git").unwrap_or(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(url: &str, description: &str) -> Repo {
        Repo {
            id: RepoId::generate(),
            workspace_id: sun_core::WorkspaceId::MAIN,
            url: url.into(),
            default_branch: "main".into(),
            gate_config: serde_json::json!({}),
            description: description.into(),
            hook_id: None,
        }
    }

    #[test]
    fn repos_are_named_by_url_segment_or_description_in_list_order() {
        let infra = repo("https://github.com/hryer/vps-infra.git", "");
        let web = repo("git@github.com:acme/web.git", "the marketing site, Svelte");
        let api = repo("https://gitlab.com/g/sub/api", "Go backend");
        let repos = vec![infra.clone(), web.clone(), api.clone()];
        for text in [
            "Harden VPS Infra firewall",
            "vps_infra: rotate keys",
            "vps-infra",
            "VPSINFRA",
        ] {
            assert_eq!(resolve_repos(text, &repos), vec![infra.id], "{text}");
        }
        // Whole tokens only: no repo named inside another word.
        let app = repo("https://github.com/acme/app.git", "");
        assert!(resolve_repos("happy path", std::slice::from_ref(&app)).is_empty());
        assert_eq!(
            resolve_repos("the app crashes", std::slice::from_ref(&app)),
            vec![app.id]
        );
        assert!(
            resolve_repos("rapid fix", &repos).is_empty(),
            "api in rapid"
        );
        assert!(
            resolve_repos("webhook retry", &repos).is_empty(),
            "web in webhook"
        );
        assert!(
            resolve_repos("infra only", &repos).is_empty(),
            "half the name"
        );
        // The whole description (words of 3+ characters), in any order.
        assert_eq!(
            resolve_repos("Fix the svelte MARKETING site header", &repos),
            vec![web.id]
        );
        assert!(
            resolve_repos("the marketing site", &repos).is_empty(),
            "not every word"
        );
        // Nothing named: nothing. Several: list order, not mention order.
        assert!(resolve_repos("Write the PRD", &repos).is_empty());
        assert_eq!(
            resolve_repos("api change, then web", &repos),
            vec![web.id, api.id]
        );
        assert!(resolve_repos("", &[]).is_empty());
        assert!(
            resolve_repos("web api", &[repo("", "")]).is_empty(),
            "no name, no words"
        );
    }
}
