//! Handoffs the user asks for. An agent hands work off with its
//! `handoff` tool, which takes a turn in its session; a session with a
//! long transcript makes that turn expensive. Here a background job on
//! the session's backend reads the transcript instead and writes the new
//! session's first prompt. The proposal shows in the old session's
//! transcript, where the user can change it, start it or decline it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use grenadine_core::api::{
    AgentBackend, AgentEvent, AgentEventKind, AgentPr, AgentSession, AnswerHandoff, GENERAL,
    NewAgent, PrKey, SessionKind, WriteHandoff, handoff_id,
};
use serde_json::json;

use crate::agent::{self, Budget, JobKind, JobSpec, JobTools};
use crate::cleanup::{self, DigestLimits};
use crate::sync::State;

/// How long writing a handoff may take before it's killed.
const TIMEOUT: Duration = Duration::from_secs(600);

/// How long asking GitHub for the session's branches may take; the
/// session waits for the handoff meanwhile.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);

/// What writing a handoff may spend.
const BUDGET: Budget = Budget { usd: 2.0, acu: 1.0 };

/// What the digest a handoff is written from keeps: more of what was
/// said than a cleanup scan's, which looks for what commands created.
const DIGEST: DigestLimits = DigestLimits {
    prompt: 8_000,
    text: 3_000,
    result: 300,
    total: 300_000,
};

/// How much of the user's instruction the job gets, in bytes.
const INSTRUCTION_LIMIT: usize = 8_000;

/// Why a handoff whose job the server didn't outlive failed.
const INTERRUPTED: &str = "the server restarted while the handoff was being written";

/// What the job is asked to do on `backend`.
fn instructions(backend: AgentBackend) -> String {
    // Devin keeps its sessions where a job can't read them.
    let tools = match backend {
        AgentBackend::Claude => {
            "Use tools only to read the full transcript, when the digest is cut short \
             or leaves out what you need."
        }
        AgentBackend::Devin => "Work from the digest alone.",
    };
    format!(
        "\
A user worked with a coding agent in the session whose transcript digest is \
below. The session's context has grown too large to go on in, so the user \
wants to hand its work off to a new session of the same agent, which starts \
with a fresh context. Write that new session's first prompt.

The new session sees only your prompt, nothing of the old session, so write \
it to stand on its own. Say what to do first, then give what the new \
session needs to know for it: the goal, what is done and what is left, the \
decisions made and why, what was tried and ruled out, how the user wants \
things done, and the files, commands, pull requests and other names that \
matter. Leave out what the task doesn't need. Write it as the user would, \
addressing the agent, in Markdown. Don't do the work yourself, and don't \
answer the old session's questions.

Where the old session's worktree is, which commit it is at and whether it \
has work that isn't committed or pushed is added below your prompt \
automatically; don't repeat it, but do say what the new session should do \
about such work when the transcript tells.

The new session starts in the same repository, on its trunk. If it should \
start from the head of one of the session's pull requests instead, give \
that pull request's number as `pr`; otherwise leave `pr` out.

{tools} You can only look; never try to change anything."
    )
}

/// The job's whole input for `session`.
fn input(
    state: &State,
    session: &AgentSession,
    events: &[AgentEvent],
    prs: &[AgentPr],
    instruction: &str,
) -> (String, Option<std::path::PathBuf>) {
    let clone = state.repo(&session.repo).map(|r| r.path);
    let place = if session.repo == GENERAL {
        format!(
            "in the directory {}, outside any repository",
            session.worktree
        )
    } else {
        format!("on {} in the worktree {}", session.repo, session.worktree)
    };
    let mut input = format!(
        "{}\n\n{}\n\nThe session \u{201c}{}\u{201d} worked {place}.\n",
        instructions(session.backend),
        cleanup::tool_notes(&session.repo, clone.as_deref()),
        session.title,
    );
    let transcript = cleanup::transcript_path(session);
    let readable = session.backend == AgentBackend::Claude && transcript.exists();
    if readable {
        input += &format!("Its full transcript is {}.\n", transcript.display());
    }
    if !prs.is_empty() {
        input += "\nIts pull requests:\n\n";
        for pr in prs {
            input += &format!(
                "- {}#{} ({}){}\n",
                pr.key.repo,
                pr.key.number,
                pr.state.to_lowercase(),
                pr.title
                    .as_deref()
                    .map(|t| format!(": {t}"))
                    .unwrap_or_default(),
            );
        }
    }
    input += "\n# What the new session should do\n\n";
    let instruction = instruction.trim();
    if instruction.is_empty() {
        input += "The user didn't say. Have it go on with what the session was working on, \
                  and with what it left open.\n";
    } else {
        input += &format!(
            "The user's own words:\n\n{}\n",
            cleanup::cut(instruction, INSTRUCTION_LIMIT)
        );
    }
    input += "\n# Transcript digest\n\n";
    input += &cleanup::digest(events, session.backend.agent_name(), &DIGEST);
    let projects = transcript.parent().filter(|_| readable).map(Path::to_owned);
    (input, projects)
}

/// Where GitHub has the branches of the session's pull requests now,
/// asked with a deadline, since the session waits for the handoff.
async fn remote_heads(state: &State, session: &AgentSession) -> Result<BTreeMap<String, String>> {
    let Some(repo) = state.repo(&session.repo) else {
        return Ok(BTreeMap::new());
    };
    let branches: Vec<String> = agent::landed_prs(session)
        .into_iter()
        .map(|pr| pr.branch)
        .collect();
    let heads = tokio::task::spawn_blocking(move || repo.remote_heads(&branches));
    match tokio::time::timeout(REMOTE_TIMEOUT, heads).await {
        Ok(heads) => heads.context("asking GitHub for the session's branches")?,
        Err(_) => bail!("GitHub didn't say where the session's branches are in time"),
    }
}

/// What the new session is told about the old session's worktree, below
/// the prompt the job wrote. `heads` are where GitHub has the branches of
/// the session's pull requests, if it could tell.
fn worktree_note(
    state: &State,
    session: &AgentSession,
    heads: Result<BTreeMap<String, String>>,
) -> String {
    let mut note = format!(
        "# The session this was handed off from\n\n\
         This prompt was written from the transcript of an earlier session, \
         \u{201c}{}\u{201d}. You can't see that session, and your worktree doesn't have \
         its commits or files unless they were pushed and you started from them.\n\n",
        session.title
    );
    let worktree = Path::new(&session.worktree);
    if !worktree.is_dir() {
        note += &format!(
            "- Its worktree was `{}`, which is gone.\n",
            session.worktree
        );
        return note;
    }
    if session.repo == GENERAL {
        note += &format!(
            "- It worked in the directory `{}`, outside any repository.\n",
            session.worktree
        );
        return note;
    }
    note += &format!("- Worktree: `{}`\n", session.worktree);
    note += &format!("- Branch: `{}`\n", session.branch);
    let Some(repo) = state.repo(&session.repo) else {
        return note;
    };
    match repo.worktree_head(worktree) {
        Ok(head) => note += &format!("- Commit: `{head}`\n"),
        Err(e) => {
            tracing::warn!("agent {}: {e:#}", session.id);
            note += "- Commit: couldn't tell\n";
            return note;
        }
    }
    note += &match repo.worktree_uncommitted_count(worktree) {
        Ok(0) => "- Uncommitted changes: none\n".to_owned(),
        Ok(1) => "- Uncommitted changes: 1 file\n".to_owned(),
        Ok(n) => format!("- Uncommitted changes: {n} files\n"),
        Err(e) => {
            tracing::warn!("agent {}: {e:#}", session.id);
            "- Uncommitted changes: couldn't tell\n".to_owned()
        }
    };
    // What GitHub has of the session's work: what it started from, the
    // trunk as last fetched, the branches of its pull requests, and every
    // head its pull requests had: one that was squash-merged and whose
    // branch is gone has its commits on the trunk only as a new one.
    let unpushed = heads.and_then(|heads| {
        let mut known: Vec<String> = heads.into_values().collect();
        for pr in agent::landed_prs(session) {
            let key = PrKey {
                repo: session.repo.clone(),
                number: pr.number,
            };
            known.extend(state.db.versions(&key)?.into_iter().map(|v| v.sha));
        }
        known.push(agent::TRUNK_REF.to_owned());
        if !session.base_sha.is_empty() {
            known.push(session.base_sha.clone());
        }
        repo.worktree_commits_beyond(worktree, &known)
    });
    note += &match unpushed {
        Ok(0) => "- Commits that aren't on GitHub: none\n".to_owned(),
        Ok(n) => format!("- Commits that aren't on GitHub: {n}\n"),
        Err(e) => {
            tracing::warn!("agent {}: {e:#}", session.id);
            "- Commits that aren't on GitHub: couldn't tell\n".to_owned()
        }
    };
    note
}

/// Whether `number` is one of `session`'s pull requests `prs`, or the
/// one it started from, in its own repository.
fn session_pr(session: &AgentSession, prs: &[AgentPr], number: u64) -> bool {
    session.pr == Some(number)
        || prs
            .iter()
            .any(|p| p.key.repo == session.repo && p.key.number == number)
}

/// Has the session's backend write the handoff and returns the session
/// it proposes.
async fn write(state: &Arc<State>, session: &AgentSession, req: &WriteHandoff) -> Result<NewAgent> {
    let events = state.db.agent_events(&session.id)?;
    let prs = state.db.agent_pr_list(&session.id)?;
    let (input, projects) = input(state, session, &events, &prs, &req.instruction);
    let clone = state.repo(&session.repo).map(|r| r.path);
    let schema = json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string"},
            "pr": {"type": "integer"},
        },
        "required": ["prompt"],
    });
    let output = state
        .agents
        .structured(
            session.backend,
            &JobSpec {
                kind: JobKind::Handoff,
                schema: &schema,
                input: &input,
                tools: JobTools::ReadOnly,
                budget: BUDGET,
                add_dir: projects.as_deref(),
                clone: clone.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await?;
    let prompt = output["prompt"].as_str().unwrap_or_default().trim();
    if prompt.is_empty() {
        bail!("the handoff has no prompt");
    }
    // A pull request that isn't one of the session's, or that it can't
    // start from, is left out rather than failing the handoff: the user
    // can still pick one in the form.
    let pr = output["pr"].as_u64().filter(|&n| {
        session_pr(session, &prs, n) && agent::check_start(state, &session.repo, Some(n)).is_ok()
    });
    agent::check_start(state, &session.repo, pr)?;
    let heads = remote_heads(state, session).await;
    let note = {
        let (state, session) = (state.clone(), session.clone());
        tokio::task::spawn_blocking(move || worktree_note(&state, &session, heads))
            .await
            .context("describing the session's worktree")?
    };
    Ok(NewAgent {
        backend: session.backend,
        repo: session.repo.clone(),
        pr,
        prompt: format!("{prompt}\n\n{note}"),
        permission_mode: req.permission_mode.clone(),
        model: req.model.clone(),
        images: Vec::new(),
        attachments: Vec::new(),
        draft: None,
    })
}

/// Why a handoff can't be written for `session` now, if it can't.
pub fn refusal(session: &AgentSession) -> Option<&'static str> {
    if session.kind != SessionKind::Interactive {
        return Some("only an interactive session can be handed off");
    }
    None
}

/// Starts writing a handoff from `session` for the user; the outcome
/// shows in its transcript. Doesn't wait for it. False while the session
/// runs a turn or has a handoff being written already.
pub fn start(state: &Arc<State>, session: &AgentSession, req: WriteHandoff) -> Result<bool> {
    if !agent::begin_handoff(state, &session.id) {
        return Ok(false);
    }
    let id = agent::approval_id("handoff");
    let requested = agent::emit(
        state,
        &session.id,
        AgentEventKind::HandoffRequested {
            id: id.clone(),
            instruction: req.instruction.trim().to_owned(),
        },
    );
    if let Err(e) = requested {
        agent::end_handoff(state, &session.id);
        return Err(e);
    }
    let (state, session) = (state.clone(), session.clone());
    tokio::spawn(async move {
        let written = tokio::select! {
            written = write(&state, &session, &req) => written,
            // The restart marks it failed.
            _ = state.shutdown.cancelled() => return,
        };
        let event = match written.and_then(|p| Ok(serde_json::to_string(&p)?)) {
            Ok(input) => AgentEventKind::HandoffProposed { id, input },
            Err(e) => {
                tracing::warn!("writing a handoff from agent {}: {e:#}", session.id);
                AgentEventKind::HandoffFailed {
                    id,
                    error: format!("{e:#}"),
                }
            }
        };
        // The session may have been deleted meanwhile.
        if matches!(state.db.agent(&session.id), Ok(Some(_)))
            && let Err(e) = agent::emit(&state, &session.id, event)
        {
            tracing::warn!("agent {}: {e:#}", session.id);
        }
        // After the proposal, so the prompts that waited for it come
        // after it in the transcript.
        agent::end_handoff(&state, &session.id);
    });
    Ok(true)
}

/// How answering a proposal went.
pub enum Answered {
    /// The session has no such proposal waiting for an answer.
    NotFound,
    Declined,
    Started(Box<AgentSession>),
}

/// Whether the transcript `events` has the proposal `proposal` and no
/// answer to it yet.
fn open(events: &[AgentEvent], proposal: &str) -> bool {
    let proposed = events
        .iter()
        .any(|e| matches!(&e.kind, AgentEventKind::HandoffProposed { id, .. } if id == proposal));
    let answered = events
        .iter()
        .any(|e| matches!(&e.kind, AgentEventKind::ApprovalResolved { id, .. } if id == proposal));
    proposed && !answered
}

/// Answers the handoff `proposal` in `session`'s transcript: starts the
/// session the user made of it, handed off from `session`, or declines
/// it.
pub async fn answer(
    state: &Arc<State>,
    session: &AgentSession,
    proposal: &str,
    answer: AnswerHandoff,
) -> Result<Answered> {
    if !open(&state.db.agent_events(&session.id)?, proposal)
        || !agent::claim_handoff(state, &session.id, proposal)
    {
        return Ok(Answered::NotFound);
    }
    let resolved = |allow| AgentEventKind::ApprovalResolved {
        id: proposal.to_owned(),
        allow,
        message: None,
        answers: None,
        expired: false,
    };
    let Some(mut req) = answer.start else {
        if let Err(e) = agent::emit(state, &session.id, resolved(false)) {
            agent::release_handoff(state, &session.id, proposal);
            return Err(e);
        }
        return Ok(Answered::Declined);
    };
    // A start that went through before the answer could be recorded,
    // e.g. before a restart, made the session with this draft id: it's
    // returned again rather than a second session started.
    req.draft = Some(handoff_id(proposal));
    let child = match agent::create_session(
        state,
        req,
        SessionKind::Interactive,
        None,
        Some(&session.id),
    )
    .await
    {
        Ok(child) => child,
        Err(e) => {
            // Nothing started, so the proposal can be answered again.
            agent::release_handoff(state, &session.id, proposal);
            return Err(e);
        }
    };
    agent::emit(state, &session.id, resolved(true))?;
    agent::emit(
        state,
        &session.id,
        AgentEventKind::HandedOff {
            id: child.id.clone(),
            title: child.title.clone(),
        },
    )?;
    Ok(Answered::Started(Box::new(child)))
}

/// Marks the handoffs that were still being written when the server
/// stopped as failed, so their transcripts don't say "writing" forever.
pub fn fail_orphaned(state: &State) -> Result<()> {
    let mut writing: Vec<(String, String)> = Vec::new();
    for (session, kind) in state.db.handoff_events()? {
        match kind {
            AgentEventKind::HandoffRequested { id, .. } => writing.push((session, id)),
            AgentEventKind::HandoffProposed { id, .. }
            | AgentEventKind::HandoffFailed { id, .. } => {
                writing.retain(|(s, w)| !(*s == session && *w == id));
            }
            _ => {}
        }
    }
    for (session, id) in writing {
        agent::emit(
            state,
            &session,
            AgentEventKind::HandoffFailed {
                id,
                error: INTERRUPTED.to_owned(),
            },
        )
        .map_err(|e| anyhow!("agent {session}: {e:#}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{AgentStatus, PermissionMode};

    use super::*;
    use crate::git::tests::{commit, fixture};

    /// A fake claude that answers a handoff job with `answer` after
    /// logging its arguments and input to `dir`, and ends turns at once.
    fn script(dir: &Path, answer: &str) -> String {
        format!(
            r#"#!/bin/sh
case " $* " in
*" --json-schema "*)
  echo "$*" > {dir}/job_args
  cat > {dir}/job_input
  {answer}
  ;;
*" --no-session-persistence "*)
  read -r request
  echo '{{"type":"control_response","response":{{"request_id":"models","response":{{"models":[{{"value":"claude-test","displayName":"Test","description":""}}]}}}}}}'
  ;;
*)
  cat > /dev/null
  echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"The cache is the culprit."}}]}},"parent_tool_use_id":null}}'
  echo '{{"type":"result","subtype":"success","is_error":false,"result":"ok"}}'
  ;;
esac
"#,
            dir = dir.display()
        )
    }

    fn request(instruction: &str) -> WriteHandoff {
        WriteHandoff {
            instruction: instruction.into(),
            permission_mode: PermissionMode::from("acceptEdits"),
            model: Some("claude-test".into()),
        }
    }

    fn kinds(state: &State, id: &str) -> Vec<AgentEventKind> {
        let events = state.db.agent_events(id).unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    /// Waits for an event `f` picks in the session's transcript.
    async fn wait_for<T>(state: &State, id: &str, f: impl Fn(&AgentEventKind) -> Option<T>) -> T {
        for _ in 0..600 {
            if let Some(found) = kinds(state, id).iter().find_map(&f) {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the event didn't come: {:?}", kinds(state, id));
    }

    /// Starts writing a handoff once the session's turn is over.
    async fn start_when_idle(state: &Arc<State>, id: &str, req: WriteHandoff) {
        wait_for(state, id, |e| {
            matches!(e, AgentEventKind::TurnEnded { .. }).then_some(())
        })
        .await;
        for _ in 0..600 {
            let session = state.db.agent(id).unwrap().unwrap();
            if session.status == AgentStatus::Idle && start(state, &session, req.clone()).unwrap() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the session stayed busy");
    }

    async fn proposal(state: &State, id: &str) -> (String, NewAgent) {
        wait_for(state, id, |e| match e {
            AgentEventKind::HandoffProposed { id, input } => {
                Some((id.clone(), serde_json::from_str(input).unwrap()))
            }
            AgentEventKind::HandoffFailed { error, .. } => panic!("{error}"),
            _ => None,
        })
        .await
    }

    /// A session on the fixture's repository, its first turn over.
    async fn session(state: &Arc<State>) -> AgentSession {
        let session = agent::create(
            state,
            NewAgent {
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: "Find out why the build is slow".into(),
                permission_mode: PermissionMode::from("auto"),
                model: None,
                images: Vec::new(),
                attachments: Vec::new(),
                draft: None,
            },
        )
        .await
        .unwrap();
        wait_for(state, &session.id, |e| {
            matches!(e, AgentEventKind::TurnEnded { .. }).then_some(())
        })
        .await;
        state.db.agent(&session.id).unwrap().unwrap()
    }

    /// A handoff the user asks for is written by a job from the
    /// transcript, with what the old worktree holds below it, and starts
    /// what the user made of the proposal, linked both ways.
    #[tokio::test]
    async fn writes_and_starts_a_proposal() {
        let fx = fixture();
        let dir = tempfile::tempdir().unwrap();
        let answer = r#"printf '%s\n' '{"type":"result","is_error":false,"structured_output":{"prompt":"Fix the cache.\n\nIt was the culprit.","pr":99}}'"#;
        let state = agent::tests::state_with_claude(&fx, dir.path(), &script(dir.path(), answer));
        let old = session(&state).await;
        let worktree = Path::new(&old.worktree);
        let head = commit(worktree, "b", "2\n");
        std::fs::write(worktree.join("notes.txt"), "todo\n").unwrap();

        start_when_idle(&state, &old.id, request(" Fix it ")).await;
        let (id, proposed) = proposal(&state, &old.id).await;
        assert!(
            kinds(&state, &old.id).contains(&AgentEventKind::HandoffRequested {
                id: id.clone(),
                instruction: "Fix it".into(),
            })
        );

        let args = std::fs::read_to_string(dir.path().join("job_args")).unwrap();
        assert!(args.starts_with("-p --model sonnet "), "{args}");
        assert!(args.contains(" --max-budget-usd 2 "), "{args}");
        assert!(args.contains(" --allowedTools Read Glob Grep "), "{args}");
        let input = std::fs::read_to_string(dir.path().join("job_input")).unwrap();
        assert!(
            input.contains("The user's own words:\n\nFix it\n"),
            "{input}"
        );
        assert!(input.contains("Find out why the build is slow"), "{input}");
        assert!(
            input.contains("Claude: The cache is the culprit."),
            "{input}"
        );

        // A pull request that can't be started from is left out.
        let note = format!(
            "- Worktree: `{}`\n- Branch: `{}`\n- Commit: `{head}`\n\
             - Uncommitted changes: 1 file\n- Commits that aren't on GitHub: 1\n",
            old.worktree, old.branch
        );
        assert!(
            proposed
                .prompt
                .starts_with("Fix the cache.\n\nIt was the culprit.\n\n# The session this"),
            "{}",
            proposed.prompt
        );
        assert!(proposed.prompt.ends_with(&note), "{}", proposed.prompt);
        assert_eq!(
            NewAgent {
                prompt: String::new(),
                ..proposed.clone()
            },
            NewAgent {
                backend: AgentBackend::Claude,
                repo: "owner/name".into(),
                pr: None,
                prompt: String::new(),
                permission_mode: PermissionMode::from("acceptEdits"),
                model: Some("claude-test".into()),
                images: Vec::new(),
                attachments: Vec::new(),
                draft: None,
            }
        );

        let edited = NewAgent {
            prompt: "Fix the cache, carefully".into(),
            ..proposed
        };
        let answered = answer_with(&state, &old, &id, Some(edited)).await;
        let Answered::Started(child) = answered else {
            panic!("the proposal didn't start");
        };
        assert_eq!(child.handed_off_from.as_deref(), Some(old.id.as_str()));
        assert_eq!(child.title, "Fix the cache, carefully");
        let events = kinds(&state, &old.id);
        assert!(events.contains(&AgentEventKind::HandedOff {
            id: child.id.clone(),
            title: child.title.clone(),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEventKind::ApprovalResolved { id: i, allow: true, expired: false, .. } if *i == id
        )));
        // It starts one session only.
        assert!(matches!(
            answer_with(&state, &old, &id, None).await,
            Answered::NotFound
        ));
    }

    /// A proposal whose start went through but wasn't recorded, e.g.
    /// before a restart, gets the session it started when it's answered
    /// again, rather than a second one.
    #[tokio::test]
    async fn a_repeated_start_gets_the_same_session() {
        let fx = fixture();
        let dir = tempfile::tempdir().unwrap();
        let answer =
            r#"echo '{"type":"result","is_error":false,"structured_output":{"prompt":"Go on."}}'"#;
        let state = agent::tests::state_with_claude(&fx, dir.path(), &script(dir.path(), answer));
        let old = session(&state).await;
        start_when_idle(&state, &old.id, request("")).await;
        let (id, proposed) = proposal(&state, &old.id).await;
        let started = agent::create_session(
            &state,
            NewAgent {
                draft: Some(handoff_id(&id)),
                ..proposed.clone()
            },
            SessionKind::Interactive,
            None,
            Some(&old.id),
        )
        .await
        .unwrap();

        let Answered::Started(child) = answer_with(&state, &old, &id, Some(proposed)).await else {
            panic!("the proposal didn't start");
        };
        assert_eq!(child.id, started.id);
        assert_eq!(state.db.agents().unwrap().len(), 2);
        assert!(kinds(&state, &old.id).contains(&AgentEventKind::HandedOff {
            id: child.id.clone(),
            title: child.title.clone(),
        }));
    }

    async fn answer_with(
        state: &Arc<State>,
        session: &AgentSession,
        proposal: &str,
        start: Option<NewAgent>,
    ) -> Answered {
        answer(state, session, proposal, AnswerHandoff { start })
            .await
            .unwrap()
    }

    /// A worktree with nothing new says so, and a declined proposal
    /// starts nothing.
    #[tokio::test]
    async fn declines_a_proposal() {
        let fx = fixture();
        let dir = tempfile::tempdir().unwrap();
        let answer =
            r#"echo '{"type":"result","is_error":false,"structured_output":{"prompt":"Go on."}}'"#;
        let state = agent::tests::state_with_claude(&fx, dir.path(), &script(dir.path(), answer));
        let old = session(&state).await;
        start_when_idle(&state, &old.id, request("")).await;
        let (id, proposed) = proposal(&state, &old.id).await;
        let input = std::fs::read_to_string(dir.path().join("job_input")).unwrap();
        assert!(input.contains("The user didn't say."), "{input}");
        assert!(
            proposed
                .prompt
                .ends_with("- Uncommitted changes: none\n- Commits that aren't on GitHub: none\n"),
            "{}",
            proposed.prompt
        );

        assert!(matches!(
            answer_with(&state, &old, "handoff-none", None).await,
            Answered::NotFound
        ));
        assert!(matches!(
            answer_with(&state, &old, &id, None).await,
            Answered::Declined
        ));
        assert!(matches!(
            answer_with(&state, &old, &id, Some(proposed)).await,
            Answered::NotFound
        ));
        assert_eq!(state.db.agents().unwrap().len(), 1);
        let events = kinds(&state, &old.id);
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEventKind::ApprovalResolved { id: i, allow: false, expired: false, .. } if *i == id
        )));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEventKind::HandedOff { .. }))
        );
    }

    /// A job that fails says why in the transcript, and another can be
    /// asked for then, but not while one is being written.
    #[tokio::test]
    async fn a_failed_job_can_be_retried() {
        let fx = fixture();
        let dir = tempfile::tempdir().unwrap();
        let answer = r#"sleep 1
  echo '{"type":"result","is_error":true,"result":"over budget"}'"#;
        let state = agent::tests::state_with_claude(&fx, dir.path(), &script(dir.path(), answer));
        let old = session(&state).await;
        start_when_idle(&state, &old.id, request("")).await;
        assert!(!start(&state, &old, request("")).unwrap());
        let error = wait_for(&state, &old.id, |e| match e {
            AgentEventKind::HandoffFailed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .await;
        assert!(error.contains("over budget"), "{error}");
        start_when_idle(&state, &old.id, request("")).await;
        let requested = kinds(&state, &old.id)
            .iter()
            .filter(|e| matches!(e, AgentEventKind::HandoffRequested { .. }))
            .count();
        assert_eq!(requested, 2);
    }

    /// Only interactive sessions are handed off, and not during a turn.
    #[tokio::test]
    async fn refuses_reviews_and_running_sessions() {
        let state = crate::sync::test_state();
        let mut session = AgentSession::fixture("s", GENERAL);
        assert_eq!(refusal(&session), None);
        session.kind = SessionKind::AutoReview;
        assert!(refusal(&session).is_some());

        let session = AgentSession::fixture("s", GENERAL);
        state.db.create_agent(&session).unwrap();
        assert!(agent::begin_handoff(&state, "s"));
        assert!(!start(&state, &session, request("")).unwrap());
        assert_eq!(kinds(&state, "s"), []);
    }

    /// Commits of a pull request that was squash-merged, its branch gone,
    /// are on GitHub, as the pull request's head once was; and a note
    /// whose branches GitHub didn't tell says it can't tell.
    #[tokio::test]
    async fn counts_a_squash_merged_prs_commits_as_on_github() {
        let fx = fixture();
        let dir = tempfile::tempdir().unwrap();
        let answer =
            r#"echo '{"type":"result","is_error":false,"structured_output":{"prompt":"Go on."}}'"#;
        let state = agent::tests::state_with_claude(&fx, dir.path(), &script(dir.path(), answer));
        let session = session(&state).await;
        let head = commit(Path::new(&session.worktree), "b", "2\n");
        let unpushed = |session: &AgentSession, heads| {
            let note = worktree_note(&state, session, heads);
            note.lines()
                .find(|l| l.starts_with("- Commits that aren't on GitHub"))
                .unwrap()
                .to_owned()
        };
        let landed = AgentSession {
            landed: Some(grenadine_core::api::Landed::PullRequest {
                number: 7,
                url: "https://github.com/owner/name/pull/7".into(),
                branch: "gone".into(),
            }),
            ..session.clone()
        };
        assert_eq!(
            unpushed(&landed, Ok(BTreeMap::new())),
            "- Commits that aren't on GitHub: 1"
        );
        state
            .db
            .store_sync(
                &crate::db::PrMeta {
                    key: PrKey {
                        repo: session.repo.clone(),
                        number: 7,
                    },
                    title: "t".into(),
                    body: String::new(),
                    author: "me".into(),
                    state: "MERGED".into(),
                    is_draft: false,
                    queued: false,
                    url: "u".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    base_ref: "main".into(),
                    head_ref: "gone".into(),
                    head_oid: head.clone(),
                    ci: None,
                    review: None,
                    review_error: None,
                },
                &[grenadine_core::api::Version {
                    number: 1,
                    sha: head,
                    merge_base: None,
                    kind: grenadine_core::api::VersionKind::Initial,
                    pushed_at: None,
                    pushed_by: None,
                    pushed_by_is_guess: false,
                    missing: false,
                }],
                &[],
                None,
                false,
                None,
            )
            .unwrap();
        assert_eq!(
            unpushed(&landed, Ok(BTreeMap::new())),
            "- Commits that aren't on GitHub: none"
        );
        assert_eq!(
            unpushed(&session, Err(anyhow!("too slow"))),
            "- Commits that aren't on GitHub: couldn't tell"
        );
    }

    /// Only the session's own pull requests in its repository can be
    /// proposed to start from.
    #[test]
    fn proposes_only_the_sessions_prs() {
        let pr = |repo: &str, number| AgentPr {
            key: grenadine_core::api::PrKey {
                repo: repo.into(),
                number,
            },
            title: None,
            state: "OPEN".into(),
            ci: None,
            conflicting: false,
            queued: false,
        };
        let session = AgentSession {
            pr: Some(1),
            ..AgentSession::fixture("s", "owner/name")
        };
        let prs = [pr("owner/name", 2), pr("owner/other", 3)];
        assert!(session_pr(&session, &prs, 1));
        assert!(session_pr(&session, &prs, 2));
        assert!(!session_pr(&session, &prs, 3));
        assert!(!session_pr(&session, &prs, 4));
    }

    /// A restart fails the handoffs that were being written, once.
    #[test]
    fn fails_the_handoffs_a_restart_interrupted() {
        let state = crate::sync::test_state();
        state
            .db
            .create_agent(&AgentSession::fixture("s", GENERAL))
            .unwrap();
        let requested = |id: &str| AgentEventKind::HandoffRequested {
            id: id.into(),
            instruction: String::new(),
        };
        let events = [
            requested("handoff-1"),
            AgentEventKind::HandoffProposed {
                id: "handoff-1".into(),
                input: "{}".into(),
            },
            requested("handoff-2"),
            AgentEventKind::HandoffFailed {
                id: "handoff-2".into(),
                error: "no".into(),
            },
            requested("handoff-3"),
        ];
        for event in events {
            agent::emit(&state, "s", event).unwrap();
        }
        fail_orphaned(&state).unwrap();
        fail_orphaned(&state).unwrap();
        let failed: Vec<_> = kinds(&state, "s")
            .into_iter()
            .filter_map(|e| match e {
                AgentEventKind::HandoffFailed { id, error } => Some((id, error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            failed,
            [
                ("handoff-2".to_owned(), "no".to_owned()),
                ("handoff-3".to_owned(), INTERRUPTED.to_owned()),
            ]
        );
    }
}
