//! Reviews of the viewer's own PRs that the server starts by itself. Each
//! poll looks for the viewer's open PRs in the repositories whose settings
//! ask for reviews. Each PR that is due gets an unattended agent session
//! that reviews it and posts the review on GitHub. Once the review is on
//! the PR, the session is deleted; the `auto_reviews` table keeps a row
//! for each review.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use grenadine_core::api::{
    AgentBackend, AgentStatus, AutoReview, AutoReviewEntry, AutoReviewState, NewAgent,
    PermissionMode, PrDetail, RepoSettings, ServerEvent, SessionKind,
};

use crate::agent;
use crate::db::Db;
use crate::github::{self, Hit};
use crate::sync::State;

/// How many reviews run at once; due PRs beyond that wait for a later
/// poll.
const MAX_RUNNING: usize = 3;

/// How long the rows of finished reviews are kept.
const KEEP: i64 = 30 * 24 * 60 * 60;

/// How long after a review's session finished its review is looked for
/// on GitHub, which can take a moment to show what was just posted.
#[cfg(not(test))]
const POSTED_CHECKS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(5),
    Duration::from_secs(20),
];
#[cfg(test)]
const POSTED_CHECKS: [Duration; 1] = [Duration::ZERO];

/// What a review's session is told beyond its prompt, whichever skill it
/// runs.
pub const NOTE: &str = "This is an unattended pull request review. Nobody reads or answers this \
    session while it runs, and it's deleted once your review is posted. Don't ask questions or \
    wait for input. Make reasonable assumptions and say what they were in your feedback. Tools \
    that need approval are denied automatically, so work without them.\n\n\
    Your review on GitHub is the only output that matters. Before you finish, post it on the \
    pull request: inline comments on the lines they're about, and a short summary. Use `gh`, \
    e.g. `gh api` for the inline comments and `gh pr review --comment` for the summary. This is \
    the author's own pull request, so post a comment review: GitHub doesn't let them approve it \
    or request changes. If the skill you were asked to run posts the review itself, don't post \
    it a second time. If you find nothing worth raising, post a short comment that says so. \
    Don't commit, push, or change the pull request's branch, title or description.";

/// The mode a review on `backend` runs in. An unattended review denies
/// every approval request, so the mode itself has to let routine
/// commands like `gh` through.
fn review_mode(backend: AgentBackend) -> PermissionMode {
    match backend {
        AgentBackend::Claude => PermissionMode::new("auto"),
        AgentBackend::Devin => PermissionMode::new("smart"),
    }
}

/// The longest skill name a repository's settings take.
const MAX_SKILL: usize = 100;

/// The skill `raw` names: trimmed and without its `/`, or `None` for a
/// blank. A name with other than letters, digits, '-', '_', ':' and '.'
/// is an error.
fn skill_name(raw: Option<&str>) -> std::result::Result<Option<String>, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(skill) => {
            let skill = skill.strip_prefix('/').unwrap_or(skill);
            let ok = !skill.is_empty()
                && skill.len() <= MAX_SKILL
                && skill
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'));
            if !ok {
                return Err(format!(
                    "{skill:?} isn't a skill name: use letters, digits, '-', '_', ':' and '.'"
                ));
            }
            Ok(Some(skill.to_owned()))
        }
    }
}

/// Checks the settings a page saves for a repository that had `old`, and
/// returns what to store. Turning reviews on starts them from `now`;
/// otherwise the old start stays.
pub fn to_save(
    old: &RepoSettings,
    new: RepoSettings,
    now: i64,
) -> std::result::Result<RepoSettings, String> {
    let review_skill = skill_name(new.review_skill.as_deref())?;
    let session_skill = skill_name(new.session_skill.as_deref())?;
    // Only real picks are stored: a blank or "default" means the
    // backend's own default.
    let review_models = new
        .review_models
        .iter()
        .filter_map(|(b, m)| {
            let m = m.trim();
            (!m.is_empty() && m != "default").then(|| (*b, m.to_owned()))
        })
        .collect();
    for (backend, mode) in &new.session_modes {
        if !backend.has_mode(mode.as_str()) {
            return Err(format!(
                "{:?} isn't one of {}'s permission modes",
                mode.as_str(),
                backend.label()
            ));
        }
    }
    let turned_on = old.auto_review == AutoReview::Off && new.auto_review != AutoReview::Off;
    let auto_review_since = match (turned_on, old.auto_review_since) {
        (false, Some(since)) => Some(since),
        _ => Some(now),
    };
    Ok(RepoSettings {
        review_skill,
        session_skill,
        review_models,
        auto_review_since,
        ..new
    })
}

/// Starts each configured repository's reviews from `now` unless they
/// already have a start, so that the PRs already open when a repository
/// is first configured aren't reviewed.
pub fn start_watching<'a>(
    db: &Db,
    repos: impl IntoIterator<Item = &'a String>,
    now: i64,
) -> Result<()> {
    for repo in repos {
        let settings = db.repo_settings(repo)?;
        if settings.auto_review_since.is_none() {
            db.set_repo_settings(
                repo,
                &RepoSettings {
                    auto_review_since: Some(now),
                    ..settings
                },
            )?;
        }
    }
    Ok(())
}

/// The viewer's open PRs in the repositories that review them.
pub async fn search(state: &State) -> Result<Vec<Hit>> {
    let mut repos = Vec::new();
    for repo in state.repos.keys() {
        if state.db.repo_settings(repo)?.auto_review != AutoReview::Off {
            repos.push(repo.clone());
        }
    }
    if repos.is_empty() {
        return Ok(Vec::new());
    }
    let query = github::search_query("is:open author:@me", &repos);
    let (_, mut pages) = state.github.search(&[(query, None, false)]).await?;
    match pages.pop() {
        Some(Ok(page)) => Ok(page.hits),
        Some(Err(e)) => Err(anyhow::anyhow!(e)),
        None => Ok(Vec::new()),
    }
}

/// Whether `hit` gets a review now. `created_at` is when the PR was
/// opened, in Unix seconds; `reviews` are its earlier reviews; `running`
/// counts the reviews of any PR that are running.
fn due(
    hit: &Hit,
    settings: &RepoSettings,
    created_at: i64,
    reviews: &[AutoReviewEntry],
    running: usize,
) -> bool {
    let wanted = match settings.auto_review {
        AutoReview::Off => false,
        AutoReview::NonDraft => !hit.is_draft,
        AutoReview::IncludingDrafts => true,
    };
    let new = settings
        .auto_review_since
        .is_some_and(|since| created_at >= since);
    let reviewed = if settings.rereview_on_push {
        reviews.iter().any(|r| r.head_oid == hit.head_oid)
    } else {
        !reviews.is_empty()
    };
    let busy = reviews.iter().any(|r| r.state == AutoReviewState::Running);
    wanted && hit.state == "OPEN" && new && !reviewed && !busy && running < MAX_RUNNING
}

/// The commit a session on the PR would start from, as `add_worktree`
/// picks it.
fn synced_head(detail: &PrDetail) -> Option<&str> {
    detail
        .versions
        .iter()
        .rev()
        .find(|v| !v.missing)
        .map(|v| v.sha.as_str())
}

/// Starts a review of each of `hits` that is due, once it has synced at
/// its head. Failures are logged; the PR gets another try next poll
/// unless its review was recorded as failed.
pub async fn start_due(state: &Arc<State>, hits: &[Hit]) {
    let now = agent::now();
    match state.db.prune_auto_reviews(now - KEEP) {
        Ok(true) => state.send(ServerEvent::AutoReviewsChanged),
        Ok(false) => {}
        Err(e) => tracing::warn!("can't prune the auto-reviews: {e:#}"),
    }
    let mut running = match state.db.auto_reviews() {
        Ok(all) => all
            .iter()
            .filter(|r| r.state == AutoReviewState::Running)
            .count(),
        Err(e) => {
            tracing::warn!("can't list the auto-reviews: {e:#}");
            return;
        }
    };
    let mut settings = BTreeMap::new();
    for hit in hits {
        let key = &hit.key;
        let checked = (|| -> Result<Option<(RepoSettings, PrDetail)>> {
            let s = match settings.get(&key.repo) {
                Some(s) => s,
                None => settings
                    .entry(key.repo.clone())
                    .or_insert(state.db.repo_settings(&key.repo)?),
            };
            let Some(created_at) = state.db.pr_created_at(key)? else {
                return Ok(None);
            };
            let reviews = state.db.auto_reviews_of(key)?;
            if !due(hit, s, github::unix(&created_at)?, &reviews, running) {
                return Ok(None);
            }
            // A sync that failed or is behind the search waits for the
            // next poll.
            Ok(state
                .db
                .pr(key)?
                .filter(|d| synced_head(d) == Some(hit.head_oid.as_str()))
                .map(|d| (s.clone(), d)))
        })();
        match checked {
            Ok(Some((s, detail))) => {
                if start(state, hit, &s, &detail).await {
                    running += 1;
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(
                "{}#{}: can't check for a review: {e:#}",
                key.repo,
                key.number
            ),
        }
    }
}

/// Starts the review of `hit`. Returns whether it's running.
async fn start(state: &Arc<State>, hit: &Hit, settings: &RepoSettings, detail: &PrDetail) -> bool {
    let key = &hit.key;
    if !state.agents.offered(settings.review_backend).await {
        tracing::warn!(
            "{}#{}: {} isn't offered, the review waits",
            key.repo,
            key.number,
            settings.review_backend.label()
        );
        return false;
    }
    let id = match state.db.start_auto_review(
        key,
        &hit.head_oid,
        &detail.summary.title,
        &detail.summary.url,
        agent::now(),
    ) {
        Ok(Some(id)) => id,
        Ok(None) => return false,
        Err(e) => {
            tracing::warn!("{}#{}: can't record a review: {e:#}", key.repo, key.number);
            return false;
        }
    };
    // A pick the backend doesn't offer fails the review rather than
    // keep it waiting: the settings were checked when they were saved,
    // so this means the backend's models changed since.
    let model = settings.review_models.get(&settings.review_backend);
    if let Some(m) = model
        && !state.agents.offers(settings.review_backend, Some(m)).await
    {
        let why = format!(
            "{} doesn't offer the model {m}",
            settings.review_backend.label()
        );
        let failed =
            state
                .db
                .finish_auto_review(id, AutoReviewState::Failed, Some(&why), agent::now());
        if let Err(e) = failed {
            tracing::warn!("{}#{}: can't record a review: {e:#}", key.repo, key.number);
        }
        state.send(ServerEvent::AutoReviewsChanged);
        return false;
    }
    tracing::info!(
        "{}#{}: starting a review of {}",
        key.repo,
        key.number,
        hit.head_oid
    );
    let req = NewAgent {
        backend: settings.review_backend,
        repo: key.repo.clone(),
        pr: Some(key.number),
        prompt: prompt(settings.review_skill.as_deref(), detail),
        permission_mode: review_mode(settings.review_backend),
        model: model.cloned(),
        images: Vec::new(),
        draft: None,
    };
    let title = format!("Review #{}: {}", key.number, detail.summary.title);
    let (running, recorded) =
        match agent::create_session(state, req, SessionKind::AutoReview, Some(title)).await {
            Ok(session) => (true, state.db.set_auto_review_session(id, &session.id)),
            Err(e) => {
                tracing::warn!("{}#{}: can't start a review: {e:#}", key.repo, key.number);
                let why = format!("{e:#}");
                let failed = AutoReviewState::Failed;
                (
                    false,
                    state
                        .db
                        .finish_auto_review(id, failed, Some(&why), agent::now()),
                )
            }
        };
    if let Err(e) = recorded {
        tracing::warn!("{}#{}: can't record a review: {e:#}", key.repo, key.number);
    }
    state.send(ServerEvent::AutoReviewsChanged);
    running
}

/// The prompt of the review of the PR in `detail`, which runs `skill`
/// when there is one. It's written so that a skill can take the PR from
/// it, and says where the PR sits in its stack.
fn prompt(skill: Option<&str>, detail: &PrDetail) -> String {
    let pr = &detail.summary;
    let mut out = String::new();
    if let Some(skill) = skill {
        out += &format!("/{skill} ");
    }
    out += &format!(
        "Review pull request {}: \"{}\" ({}#{}).\n\n\
         The working directory is checked out at the pull request's head. `gh pr diff {}` \
         shows its changes.",
        pr.url, pr.title, pr.key.repo, pr.key.number, pr.key.number
    );
    if let Some(stack) = stack_lines(detail) {
        out += &format!(
            " The pull request is part of a stack, bottom first:\n\n{stack}\n\n\
             Look at the pull requests below this one for the context its changes build on, \
             and at the ones above it for how its changes get used. Review only this pull \
             request's own changes, but use the rest of the stack to judge them."
        );
    }
    out
}

/// The PRs of the stack that `detail`'s PR is in, bottom first, each
/// saying where it sits; `None` when the PR isn't stacked.
fn stack_lines(detail: &PrDetail) -> Option<String> {
    let stack = detail.stack.as_ref().filter(|s| s.prs.len() > 1)?;
    let this = detail.summary.key.number;
    let parent: BTreeMap<u64, Option<u64>> =
        stack.prs.iter().map(|p| (p.number, p.parent)).collect();
    // How many PRs are below each one, and whether this PR is one of them.
    let walk = |mut n: u64| {
        let (mut depth, mut above_this) = (0, false);
        while let Some(Some(p)) = parent.get(&n) {
            depth += 1;
            above_this |= *p == this;
            n = *p;
            if depth > stack.prs.len() {
                break;
            }
        }
        (depth, above_this)
    };
    let mut ancestors = Vec::new();
    let mut n = this;
    while let Some(Some(p)) = parent.get(&n) {
        if ancestors.contains(p) {
            break;
        }
        ancestors.push(*p);
        n = *p;
    }
    let mut prs: Vec<_> = stack.prs.iter().map(|p| (walk(p.number), p)).collect();
    prs.sort_by_key(|((depth, _), p)| (*depth, p.number));
    let lines: Vec<String> = prs
        .into_iter()
        .map(|((_, above_this), p)| {
            let place = if p.number == this {
                "this pull request"
            } else if ancestors.contains(&p.number) {
                "below: this pull request builds on it"
            } else if above_this {
                "above: builds on this pull request"
            } else {
                "elsewhere in the stack"
            };
            let merged = if p.state == "MERGED" { ", merged" } else { "" };
            format!("- #{} {} ({place}{merged}) {}", p.number, p.title, p.url)
        })
        .collect();
    Some(format!(
        "Base branch: {}\n{}",
        stack.base_ref,
        lines.join("\n")
    ))
}

/// Whether the viewer posted a review, a review comment or a comment on
/// the PR at or after `since`, Unix seconds.
fn posted_since(detail: &PrDetail, since: i64) -> bool {
    let Some(review) = &detail.review else {
        return false;
    };
    let viewer = review.viewer.as_str();
    let after = |ts: &str| github::unix(ts).is_ok_and(|t| t >= since);
    let by_viewer = |login: &Option<String>| login.as_deref() == Some(viewer);
    review
        .reviews
        .iter()
        .any(|r| by_viewer(&r.author.login) && after(&r.submitted_at))
        || review
            .comments
            .iter()
            .any(|c| by_viewer(&c.author.login) && after(&c.created_at))
        || detail
            .comments
            .iter()
            .any(|c| c.author == viewer && !c.pending && after(&c.created_at))
}

/// Goes on with the review of session `id` once its turns are done and it
/// ended up `last`.
pub fn session_done(state: &Arc<State>, id: &str, last: AgentStatus) {
    match state.db.agent(id) {
        Ok(Some(s)) if s.kind == SessionKind::AutoReview => {}
        Ok(_) => return,
        Err(e) => {
            tracing::warn!("agent {id}: {e:#}");
            return;
        }
    }
    // Its own task, since deleting the session stops the task that runs
    // its turns, which calls this.
    let (state, id) = (state.clone(), id.to_owned());
    tokio::spawn(async move {
        // A turn that's over quickly can end before `start` recorded
        // which review the session is doing.
        let mut review = None;
        for _ in 0..50 {
            match state.db.auto_review_of(&id) {
                Ok(Some(r)) => {
                    review = Some(r);
                    break;
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(e) => {
                    tracing::warn!("agent {id}: {e:#}");
                    return;
                }
            }
        }
        let Some(review) = review.filter(|r| r.state == AutoReviewState::Running) else {
            return;
        };
        if let Err(e) = finish(&state, &id, &review, last).await {
            tracing::warn!("agent {id}: can't finish the review: {e:#}");
        }
        state.send(ServerEvent::AutoReviewsChanged);
    });
}

async fn finish(
    state: &Arc<State>,
    id: &str,
    review: &AutoReviewEntry,
    last: AgentStatus,
) -> Result<()> {
    let failed = |why: &str| {
        state
            .db
            .finish_auto_review(review.id, AutoReviewState::Failed, Some(why), agent::now())
    };
    // A session stopped before its turn ran is idle too.
    let answered = state.db.agent_events(id)?.iter().any(|e| {
        matches!(
            e.kind,
            grenadine_core::api::AgentEventKind::TurnEnded { error: None, .. }
        )
    });
    match last {
        AgentStatus::Idle if answered => {}
        AgentStatus::Idle | AgentStatus::Interrupted => {
            return failed("The review was interrupted.");
        }
        _ => return failed("The review's session failed."),
    }
    for delay in POSTED_CHECKS {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = state.shutdown.cancelled() => return Ok(()),
        }
        if !state.resync_now(&review.key).await {
            continue;
        }
        if state
            .db
            .pr(&review.key)?
            .is_some_and(|d| posted_since(&d, review.started_at))
        {
            state
                .db
                .finish_auto_review(review.id, AutoReviewState::Posted, None, agent::now())?;
            tracing::info!(
                "{}#{}: the review is posted",
                review.key.repo,
                review.key.number
            );
            agent::delete(state, id).await?;
            return Ok(());
        }
    }
    state
        .db
        .finish_auto_review(review.id, AutoReviewState::Unposted, None, agent::now())
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{
        CostUnit, IssueComment, MergeInfo, MergeState, Person, PrKey, PrSummary, Review,
        ReviewInfo, ReviewState, Stack, StackPr, ThreadCounts, Version, VersionKind,
    };

    use super::*;

    fn hit(draft: bool, head: &str) -> Hit {
        Hit {
            key: PrKey {
                repo: "o/n".into(),
                number: 2,
            },
            title: "t".into(),
            author: "me".into(),
            state: "OPEN".into(),
            is_draft: draft,
            url: "u".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            head_oid: head.into(),
            threads: ThreadCounts::default(),
            ci: None,
            review_decision: None,
            facts: Default::default(),
        }
    }

    fn entry(head: &str, state: AutoReviewState) -> AutoReviewEntry {
        AutoReviewEntry {
            id: 1,
            key: PrKey {
                repo: "o/n".into(),
                number: 2,
            },
            title: "t".into(),
            url: "u".into(),
            head_oid: head.into(),
            state,
            session: None,
            status: None,
            started_at: 0,
            finished_at: None,
            cost: 0.0,
            cost_unit: CostUnit::Usd,
            error: None,
        }
    }

    fn settings(mode: AutoReview, rereview: bool) -> RepoSettings {
        RepoSettings {
            review_skill: None,
            auto_review: mode,
            rereview_on_push: rereview,
            auto_review_since: Some(100),
            ..RepoSettings::default()
        }
    }

    #[test]
    fn due_prs() {
        let s = settings(AutoReview::NonDraft, false);
        assert!(due(&hit(false, "a"), &s, 100, &[], 0));
        // Opened before reviews started, a draft, or too many running.
        assert!(!due(&hit(false, "a"), &s, 99, &[], 0));
        assert!(!due(&hit(true, "a"), &s, 100, &[], 0));
        assert!(!due(&hit(false, "a"), &s, 100, &[], MAX_RUNNING));
        let s = settings(AutoReview::IncludingDrafts, false);
        assert!(due(&hit(true, "a"), &s, 100, &[], 0));
        let s = settings(AutoReview::Off, false);
        assert!(!due(&hit(false, "a"), &s, 100, &[], 0));
        let s = RepoSettings {
            auto_review_since: None,
            ..settings(AutoReview::NonDraft, false)
        };
        assert!(!due(&hit(false, "a"), &s, 100, &[], 0));

        // Without re-reviews, a PR is reviewed once.
        let s = settings(AutoReview::NonDraft, false);
        let done = [entry("a", AutoReviewState::Posted)];
        assert!(!due(&hit(false, "b"), &s, 100, &done, 0));
        // With them, each head is, but not while a review is running.
        let s = settings(AutoReview::NonDraft, true);
        assert!(!due(&hit(false, "a"), &s, 100, &done, 0));
        assert!(due(&hit(false, "b"), &s, 100, &done, 0));
        let running = [entry("a", AutoReviewState::Running)];
        assert!(!due(&hit(false, "b"), &s, 100, &running, 0));
    }

    #[test]
    fn saving_settings() {
        let off = RepoSettings {
            auto_review: AutoReview::Off,
            auto_review_since: Some(5),
            ..RepoSettings::default()
        };
        let on = RepoSettings {
            review_skill: Some(" /code-review ".into()),
            auto_review_since: Some(1),
            ..RepoSettings::default()
        };
        // Turning reviews on starts them now; the page's start is ignored.
        let saved = to_save(&off, on.clone(), 50).unwrap();
        assert_eq!(saved.review_skill.as_deref(), Some("code-review"));
        assert_eq!(saved.auto_review_since, Some(50));
        // Other changes keep the start.
        let saved = to_save(&saved, on, 60).unwrap();
        assert_eq!(saved.auto_review_since, Some(50));
        let saved = to_save(&saved, off.clone(), 70).unwrap();
        assert_eq!(saved.auto_review_since, Some(50));

        let blank = RepoSettings {
            review_skill: Some("  ".into()),
            ..off.clone()
        };
        assert_eq!(to_save(&off, blank, 0).unwrap().review_skill, None);
        for bad in ["/", "rm -rf", "a\nb", &"x".repeat(MAX_SKILL + 1)] {
            let s = RepoSettings {
                review_skill: Some(bad.into()),
                ..off.clone()
            };
            assert!(to_save(&off, s, 0).is_err(), "{bad:?}");
        }
        let plugin = RepoSettings {
            review_skill: Some("plugin:review_v2.1".into()),
            ..off.clone()
        };
        assert!(to_save(&off, plugin, 0).is_ok());
    }

    #[test]
    fn saving_session_defaults() {
        let old = RepoSettings::default();
        let with = |skill: Option<&str>, modes: &[(&str, &str)]| RepoSettings {
            session_skill: skill.map(Into::into),
            session_modes: modes
                .iter()
                .map(|(b, m)| {
                    (
                        grenadine_core::api::AgentBackend::parse(b).unwrap(),
                        PermissionMode::new(*m),
                    )
                })
                .collect(),
            ..RepoSettings::default()
        };
        let saved = to_save(&old, with(Some(" /foo "), &[]), 0).unwrap();
        assert_eq!(saved.session_skill.as_deref(), Some("foo"));
        assert_eq!(
            to_save(&old, with(Some("  "), &[]), 0)
                .unwrap()
                .session_skill,
            None
        );
        assert!(to_save(&old, with(Some("a b"), &[]), 0).is_err());
        // A mode the backend doesn't have is rejected.
        assert!(to_save(&old, with(None, &[("devin", "auto")]), 0).is_err());
        let saved = to_save(&old, with(None, &[("claude", "acceptEdits")]), 0).unwrap();
        assert_eq!(
            saved.session_modes[&grenadine_core::api::AgentBackend::Claude].as_str(),
            "acceptEdits"
        );
    }

    #[test]
    fn review_modes() {
        assert_eq!(review_mode(AgentBackend::Claude).as_str(), "auto");
        assert_eq!(review_mode(AgentBackend::Devin).as_str(), "smart");
    }

    #[test]
    fn settings_from_before_the_review_backend() {
        // Settings saved before `review_backend` existed review with
        // Claude.
        let old: RepoSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(old.review_backend, AgentBackend::Claude);
    }

    #[test]
    fn saving_the_review_backend() {
        let off = RepoSettings {
            auto_review: AutoReview::Off,
            ..RepoSettings::default()
        };
        let devin = RepoSettings {
            review_backend: AgentBackend::Devin,
            ..off.clone()
        };
        let saved = to_save(&off, devin, 0).unwrap();
        assert_eq!(saved.review_backend, AgentBackend::Devin);
    }

    #[test]
    fn saving_the_review_models() {
        let off = RepoSettings {
            auto_review: AutoReview::Off,
            ..RepoSettings::default()
        };
        // Only real picks are kept, trimmed; "default" and blanks go.
        let models = RepoSettings {
            review_models: [
                (AgentBackend::Claude, " sonnet ".to_owned()),
                (AgentBackend::Devin, "default".to_owned()),
            ]
            .into_iter()
            .collect(),
            ..off.clone()
        };
        let saved = to_save(&off, models, 0).unwrap();
        assert_eq!(saved.review_models.len(), 1);
        assert_eq!(saved.review_models[&AgentBackend::Claude], "sonnet");
    }

    #[test]
    fn watching_starts_once() {
        let db = Db::in_memory();
        let repos = ["o/n".to_owned()];
        start_watching(&db, &repos, 10).unwrap();
        start_watching(&db, &repos, 20).unwrap();
        assert_eq!(db.repo_settings("o/n").unwrap().auto_review_since, Some(10));
    }

    fn stack_pr(number: u64, parent: Option<u64>, state: &str) -> StackPr {
        StackPr {
            number,
            title: format!("PR {number}"),
            state: state.into(),
            is_draft: false,
            url: format!("https://github.com/o/n/pull/{number}"),
            updated_at: String::new(),
            head_oid: String::new(),
            parent,
        }
    }

    fn detail(number: u64, stack: Option<Stack>) -> PrDetail {
        PrDetail {
            summary: PrSummary {
                key: PrKey {
                    repo: "o/n".into(),
                    number,
                },
                title: format!("PR {number}"),
                author: "me".into(),
                state: "OPEN".into(),
                is_draft: false,
                updated_at: String::new(),
                url: format!("https://github.com/o/n/pull/{number}"),
                version_count: 1,
                synced: true,
                sync_error: None,
                stack: None,
                threads: None,
                ci: None,
                review_decision: None,
            },
            body: String::new(),
            base_ref: "main".into(),
            head_ref: "b".into(),
            versions: Vec::new(),
            comments: Vec::new(),
            stack,
            approximate: false,
            drift: None,
            sync_error: None,
            review: None,
        }
    }

    #[test]
    fn prompts() {
        let lone = detail(7, None);
        assert_eq!(
            prompt(None, &lone),
            "Review pull request https://github.com/o/n/pull/7: \"PR 7\" (o/n#7).\n\n\
             The working directory is checked out at the pull request's head. `gh pr diff 7` \
             shows its changes."
        );
        assert!(prompt(Some("code-review"), &lone).starts_with("/code-review Review pull request"));

        let stacked = detail(
            42,
            Some(Stack {
                base_ref: "main".into(),
                prs: vec![
                    stack_pr(43, Some(42), "OPEN"),
                    stack_pr(42, Some(41), "OPEN"),
                    stack_pr(41, None, "MERGED"),
                    stack_pr(44, Some(43), "OPEN"),
                ],
                more_ancestors: false,
                more_descendants: false,
            }),
        );
        let text = prompt(None, &stacked);
        let list = "Base branch: main\n\
            - #41 PR 41 (below: this pull request builds on it, merged) https://github.com/o/n/pull/41\n\
            - #42 PR 42 (this pull request) https://github.com/o/n/pull/42\n\
            - #43 PR 43 (above: builds on this pull request) https://github.com/o/n/pull/43\n\
            - #44 PR 44 (above: builds on this pull request) https://github.com/o/n/pull/44";
        assert!(text.contains(list), "{text}");
        assert!(text.contains("Look at the pull requests below this one"));
    }

    /// Starts a review session of the trunk that runs `script` as claude,
    /// recorded as reviewing #2.
    async fn review_session(
        script: &str,
    ) -> (
        crate::git::tests::Fixture,
        tempfile::TempDir,
        Arc<State>,
        String,
    ) {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(&fx, dir.path(), script);
        let key = PrKey {
            repo: fx.clone.slug.clone(),
            number: 2,
        };
        let review = state
            .db
            .start_auto_review(&key, "abc", "t", "u", agent::now())
            .unwrap()
            .unwrap();
        let req = NewAgent {
            backend: AgentBackend::Claude,
            repo: fx.clone.slug.clone(),
            pr: None,
            prompt: "Review it".into(),
            permission_mode: PermissionMode::new("auto"),
            model: None,
            images: Vec::new(),
            draft: None,
        };
        let title = Some("Review #2: t".to_owned());
        let session = agent::create_session(&state, req, SessionKind::AutoReview, title)
            .await
            .unwrap();
        assert_eq!(session.title, "Review #2: t");
        state
            .db
            .set_auto_review_session(review, &session.id)
            .unwrap();
        (fx, dir, state, session.id)
    }

    /// Waits for the only review to end.
    async fn ended(state: &State) -> AutoReviewEntry {
        for _ in 0..300 {
            let review = state.db.auto_reviews().unwrap().remove(0);
            if review.state != AutoReviewState::Running {
                return review;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the review didn't end");
    }

    const SUCCEED: &str = r#"#!/bin/sh
echo "$*" > "$(dirname "$0")/args"
echo '{"type":"system","subtype":"init","session_id":"s","model":"claude-test"}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Done","total_cost_usd":0.5}'
"#;

    /// A session that finished without anything on the PR is kept, with
    /// its cost.
    #[tokio::test]
    async fn a_review_that_posted_nothing_is_kept() {
        let (_fx, dir, state, id) = review_session(SUCCEED).await;
        let review = ended(&state).await;
        assert_eq!(review.state, AutoReviewState::Unposted);
        assert_eq!(review.session.as_deref(), Some(id.as_str()));
        assert_eq!(review.cost, 0.5);
        // The session was told it runs unattended.
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert!(
            args.contains("This is an unattended pull request review."),
            "{args}"
        );
        let session = state.db.agent(&id).unwrap().unwrap();
        assert_eq!(session.kind, SessionKind::AutoReview);

        // Deleting it keeps the review and its cost.
        assert!(agent::delete(&state, &id).await.unwrap());
        let review = state.db.auto_reviews().unwrap().remove(0);
        assert_eq!(review.state, AutoReviewState::Unposted);
        assert_eq!((review.session, review.cost), (None, 0.5));
        // A review leaves nothing to clean up.
        let cleanup = crate::cleanup::cleanup_state(&state).unwrap();
        assert!(cleanup.scanning.is_empty() && cleanup.items.is_empty());
    }

    #[tokio::test]
    async fn a_failed_review_is_kept() {
        let script = r#"#!/bin/sh
echo '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}'
"#;
        let (_fx, _dir, state, id) = review_session(script).await;
        let review = ended(&state).await;
        assert_eq!(review.state, AutoReviewState::Failed);
        assert_eq!(
            review.error.as_deref(),
            Some("The review's session failed.")
        );
        assert!(state.db.agent(&id).unwrap().is_some());
    }

    /// Deleting a running review cancels it.
    #[tokio::test]
    async fn deleting_a_running_review_cancels_it() {
        let script = "#!/bin/sh
sleep 30
";
        let (_fx, _dir, state, id) = review_session(script).await;
        assert!(agent::delete(&state, &id).await.unwrap());
        let review = state.db.auto_reviews().unwrap().remove(0);
        assert_eq!(review.state, AutoReviewState::Cancelled);
        assert!(review.finished_at.is_some());
    }

    /// Nobody answers an unattended session's requests, so they're denied
    /// right away, in the transcript too.
    #[tokio::test]
    async fn requests_are_denied() {
        let script = "#!/bin/sh
sleep 30
";
        let (_fx, _dir, state, id) = review_session(script).await;
        let input = serde_json::json!({"command": "rm -rf /"});
        let decision = agent::await_approval(&state, &id, "t1", "Bash", &input, Vec::new())
            .await
            .unwrap();
        assert!(!decision.allow);
        assert_eq!(decision.message.as_deref(), Some(agent::UNATTENDED_DENIAL));
        let events: Vec<_> = state
            .db
            .agent_events(&id)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(events.iter().any(|e| matches!(
            e,
            grenadine_core::api::AgentEventKind::ApprovalResolved { allow: false, .. }
        )));
        assert_ne!(
            state.db.agent(&id).unwrap().unwrap().status,
            AgentStatus::AwaitingApproval
        );
        agent::delete(&state, &id).await.unwrap();
    }

    /// Starts a Devin review session built the way `start` builds one,
    /// recorded as reviewing #2; the fake devin plays `steps` for the
    /// review's prompt.
    async fn devin_review_session(
        steps: Vec<serde_json::Value>,
    ) -> (
        crate::git::tests::Fixture,
        tempfile::TempDir,
        Arc<State>,
        String,
    ) {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let devin = crate::devin::fake(
            dir.path(),
            serde_json::json!({
                "session_id": "review-devin",
                "model": "swe-2",
                "models": [{"value": "swe-2", "name": "SWE-2"}],
                "prompts": [{"match": "Review pull request", "steps": steps}],
            }),
        );
        let state = crate::agent::devin_tests::state_with_devin(&fx, dir.path(), devin);
        let key = PrKey {
            repo: fx.clone.slug.clone(),
            number: 2,
        };
        let review = state
            .db
            .start_auto_review(&key, "abc", "t", "u", agent::now())
            .unwrap()
            .unwrap();
        let req = NewAgent {
            backend: AgentBackend::Devin,
            repo: fx.clone.slug.clone(),
            pr: None,
            prompt: prompt(None, &detail(2, None)),
            permission_mode: review_mode(AgentBackend::Devin),
            model: None,
            images: Vec::new(),
            draft: None,
        };
        let title = Some("Review #2: t".to_owned());
        let session = agent::create_session(&state, req, SessionKind::AutoReview, title)
            .await
            .unwrap();
        state
            .db
            .set_auto_review_session(review, &session.id)
            .unwrap();
        (fx, dir, state, session.id)
    }

    /// A Devin review runs in smart mode and its cost is in ACUs.
    #[tokio::test]
    async fn a_devin_review() {
        let steps = vec![
            serde_json::json!({"update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "Reviewed."},
            }}),
            serde_json::json!({"update": {
                "sessionUpdate": "usage_update",
                "used": 100,
                "size": 200000,
                "_meta": {"cognition.ai/totalAcuCost": 0.25},
            }}),
            serde_json::json!({"end": "end_turn"}),
        ];
        let (_fx, dir, state, id) = devin_review_session(steps).await;
        let review = ended(&state).await;
        assert_eq!(review.state, AutoReviewState::Unposted);
        assert_eq!(review.session.as_deref(), Some(id.as_str()));
        assert_eq!(review.cost, 0.25);
        assert_eq!(review.cost_unit, CostUnit::Acu);
        let session = state.db.agent(&id).unwrap().unwrap();
        assert_eq!(session.backend, AgentBackend::Devin);
        let mode = crate::devin::fake_log(dir.path())
            .into_iter()
            .find(|m| m["method"] == "session/set_mode")
            .expect("the review's mode was set");
        assert_eq!(mode["params"]["modeId"], "smart");
    }

    /// A request a Devin review is denied doesn't come back as another
    /// turn: the review ends with the one prompt it ran.
    #[tokio::test]
    async fn a_devin_reviews_denial_isnt_queued() {
        let steps = vec![
            serde_json::json!({"update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "exec:0#t",
                "rawInput": {"command": "gh pr review --comment"},
                "_meta": {"cognition.ai/inferenceToolName": "exec"},
            }}),
            serde_json::json!({
                "request": "session/request_permission",
                "params": {
                    "sessionId": "review-devin",
                    "toolCall": {"toolCallId": "exec:0#t"},
                    "options": [
                        {"optionId": "allow_once", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                    ],
                },
            }),
            serde_json::json!({"end": "end_turn"}),
        ];
        let (_fx, dir, state, _id) = devin_review_session(steps).await;
        let review = ended(&state).await;
        assert_eq!(review.state, AutoReviewState::Unposted);
        let prompts: Vec<_> = crate::devin::fake_log(dir.path())
            .into_iter()
            .filter(|m| m["method"] == "session/prompt")
            .collect();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
    }

    /// While the review's backend isn't offered, the review isn't
    /// recorded and stays due for a later poll.
    #[tokio::test]
    async fn a_review_waits_for_its_backend() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        // devin's path doesn't exist, so Devin isn't offered.
        let state =
            crate::agent::devin_tests::state_with_devin(&fx, dir.path(), dir.path().join("devin"));
        let s = RepoSettings {
            review_backend: AgentBackend::Devin,
            ..settings(AutoReview::NonDraft, false)
        };
        assert!(!start(&state, &hit(false, "a"), &s, &detail(2, None)).await);
        assert!(state.db.auto_reviews().unwrap().is_empty());
    }

    /// A review runs on the model its repository's settings picked.
    #[tokio::test]
    async fn a_review_runs_on_the_picked_model() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(&fx, dir.path(), SUCCEED);
        let key = PrKey {
            repo: fx.clone.slug.clone(),
            number: 2,
        };
        // The PR synced at a commit the clone has.
        let head = fx.clone.head().unwrap();
        state
            .db
            .store_sync(
                &crate::db::PrMeta {
                    key: key.clone(),
                    title: "t".into(),
                    body: String::new(),
                    author: "me".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    url: "u".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    base_ref: "main".into(),
                    head_ref: "b".into(),
                    head_oid: head.clone(),
                    ci: None,
                    review: None,
                    review_error: None,
                },
                &[Version {
                    number: 1,
                    sha: head.clone(),
                    merge_base: None,
                    kind: VersionKind::Initial,
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
        let detail = state.db.pr(&key).unwrap().unwrap();
        let s = RepoSettings {
            review_models: [(AgentBackend::Claude, "sonnet".to_owned())]
                .into_iter()
                .collect(),
            ..settings(AutoReview::NonDraft, false)
        };
        let mut h = hit(false, &head);
        h.key = key;
        assert!(start(&state, &h, &s, &detail).await);
        let review = ended(&state).await;
        assert_eq!(review.state, AutoReviewState::Unposted);
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert!(args.contains("--model sonnet"), "{args}");
    }

    /// A review whose picked model the backend doesn't list is recorded
    /// failed, without a session.
    #[tokio::test]
    async fn a_review_with_an_unoffered_model_fails() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(
            &fx,
            dir.path(),
            crate::claude::tests::LISTS_SONNET,
        );
        let s = RepoSettings {
            review_models: [(AgentBackend::Claude, "nope".to_owned())]
                .into_iter()
                .collect(),
            ..settings(AutoReview::NonDraft, false)
        };
        assert!(!start(&state, &hit(false, "a"), &s, &detail(2, None)).await);
        let review = state.db.auto_reviews().unwrap().remove(0);
        assert_eq!(review.state, AutoReviewState::Failed);
        assert_eq!(
            review.error.as_deref(),
            Some("Claude Code doesn't offer the model nope")
        );
        assert_eq!(review.session, None);
        assert!(state.db.agents().unwrap().is_empty());
    }

    #[test]
    fn reviews_are_recorded_once_per_head() {
        let db = Db::in_memory();
        let key = PrKey {
            repo: "o/n".into(),
            number: 2,
        };
        let id = db
            .start_auto_review(&key, "a", "t", "u", 10)
            .unwrap()
            .unwrap();
        assert_eq!(db.start_auto_review(&key, "a", "t", "u", 11).unwrap(), None);
        assert!(
            db.start_auto_review(&key, "b", "t", "u", 12)
                .unwrap()
                .is_some()
        );
        assert_eq!(db.auto_reviews_of(&key).unwrap().len(), 2);

        // A server that stopped during a review failed it; ending it again
        // changes nothing.
        db.interrupt_auto_reviews(20).unwrap();
        db.finish_auto_review(id, AutoReviewState::Posted, None, 30)
            .unwrap();
        let all = db.auto_reviews().unwrap();
        assert!(all.iter().all(|r| r.state == AutoReviewState::Failed));
        assert!(all.iter().all(|r| r.finished_at == Some(20)));

        // Old rows without a session go.
        assert!(!db.prune_auto_reviews(20).unwrap());
        assert!(db.prune_auto_reviews(21).unwrap());
        assert!(db.auto_reviews().unwrap().is_empty());
    }

    fn person(login: &str) -> Person {
        Person {
            login: Some(login.into()),
            name: None,
            avatar_url: None,
        }
    }

    #[test]
    fn what_counts_as_posted() {
        let since = github::unix("2026-01-01T00:00:00Z").unwrap();
        let mut d = detail(1, None);
        assert!(!posted_since(&d, since));
        let mut info = ReviewInfo {
            viewer: "me".into(),
            decision: None,
            reviews: vec![],
            requested: vec![],
            comments: vec![],
            checks: vec![],
            ci: None,
            pending_review: None,
            merge: MergeInfo {
                conflicting: None,
                state: MergeState::Clean,
                methods: vec![],
                default_method: None,
                delete_branch_on_merge: false,
                can_merge_as_admin: false,
                requires_conversation_resolution: false,
            },
        };
        // Someone else's review, or the viewer's from before, don't count.
        info.reviews.push(Review {
            author: person("other"),
            state: ReviewState::Commented,
            body: String::new(),
            submitted_at: "2026-01-02T00:00:00Z".into(),
            url: String::new(),
            commit: None,
        });
        info.reviews.push(Review {
            author: person("me"),
            state: ReviewState::Commented,
            body: String::new(),
            submitted_at: "2025-12-31T00:00:00Z".into(),
            url: String::new(),
            commit: None,
        });
        d.review = Some(info.clone());
        assert!(!posted_since(&d, since));
        info.comments.push(IssueComment {
            author: person("me"),
            body: "LGTM".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            url: String::new(),
        });
        d.review = Some(info);
        assert!(posted_since(&d, since));
    }
}
