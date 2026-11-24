//! The commands a prompt can start with, as `/name`, for the page to
//! suggest: the ones the backend listed for the session, or for the
//! repository's latest session, with how often the repository's prompts
//! used each.

use std::path::Path;

use anyhow::Result;
use grenadine_core::api::{AgentBackend, GENERAL, SlashCommand, command_char};

use crate::sync::State;

/// The commands for a prompt in `repo` that `backend` runs: those of
/// `session` once its backend listed them, else those the repository's
/// latest session of the backend listed, else, before any has, the
/// skills and commands found in Claude Code's directories.
pub fn list(
    state: &State,
    repo: &str,
    backend: AgentBackend,
    session: Option<&str>,
) -> Result<Vec<SlashCommand>> {
    let listed = match session {
        Some(id) => state.db.agent_slash_commands(id)?,
        None => None,
    };
    let names = match listed {
        Some(names) => names,
        None => match state.db.repo_slash_commands(repo, backend)? {
            Some(names) => names,
            None if backend == AgentBackend::Claude => {
                let checkout = (repo != GENERAL)
                    .then(|| state.repos.get(repo).map(|r| r.repo.path.clone()))
                    .flatten();
                scan(&crate::claude::config_dir(), checkout.as_deref())
            }
            None => Vec::new(),
        },
    };
    state.db.with_uses(repo, names)
}

/// The skills and commands in the Claude Code config directory `config`
/// and in the repository checked out at `checkout`, sorted.
/// It doesn't read the skills' frontmatter, so ones that aren't
/// user-invocable are included until a session lists its commands.
fn scan(config: &Path, checkout: Option<&Path>) -> Vec<String> {
    let mut names = Vec::new();
    let dirs = std::iter::once(config.to_owned()).chain(checkout.map(|c| c.join(".claude")));
    for dir in dirs {
        // A skill is a directory with a SKILL.md, named after the
        // directory; a command is a markdown file, named after the file.
        names.extend(
            entries(&dir.join("skills"))
                .filter_map(|(name, path)| path.join("SKILL.md").is_file().then_some(name)),
        );
        names.extend(entries(&dir.join("commands")).filter_map(|(name, path)| {
            let name = name.strip_suffix(".md")?;
            path.is_file().then(|| name.to_owned())
        }));
    }
    names.retain(|n| !n.is_empty() && n.chars().all(command_char));
    names.sort();
    names.dedup();
    names
}

/// The names and paths of what's in `dir`; nothing if it can't be read.
fn entries(dir: &Path) -> impl Iterator<Item = (String, std::path::PathBuf)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_skills_and_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let checkout = tmp.path().join("checkout");
        let mkskill = |dir: &Path, name: &str| {
            std::fs::create_dir_all(dir.join("skills").join(name)).unwrap();
            std::fs::write(dir.join("skills").join(name).join("SKILL.md"), "").unwrap();
        };
        mkskill(&config, "foo");
        mkskill(&checkout.join(".claude"), "bar");
        mkskill(&checkout.join(".claude"), "foo");
        // Not a skill without its SKILL.md.
        std::fs::create_dir_all(config.join("skills/empty")).unwrap();
        std::fs::create_dir_all(config.join("commands")).unwrap();
        std::fs::write(config.join("commands/baz.md"), "").unwrap();
        std::fs::write(config.join("commands/notes.txt"), "").unwrap();
        std::fs::write(config.join("commands/has space.md"), "").unwrap();
        assert_eq!(scan(&config, Some(&checkout)), ["bar", "baz", "foo"]);
        assert_eq!(scan(&config, None), ["baz", "foo"]);
        assert!(scan(&tmp.path().join("missing"), None).is_empty());
    }
}
