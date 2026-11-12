//! Reviews of the viewer's own PRs that the server starts by itself. Each
//! poll looks for the viewer's open PRs in the repositories whose settings
//! ask for reviews. Each PR that is due gets an unattended agent session
//! that reviews it and posts the review on GitHub. Once the review is on
//! the PR, the session is deleted; the `auto_reviews` table keeps a row
//! for each review.
//!
//! A review can also be asked for from the PR's page, whatever the
//! repository's settings say. It's queued, and starts as soon as fewer
//! than `MAX_RUNNING` reviews run, before the poller starts any. A PR has
//! at most one review queued or running at a time.
//!
//! A fixing run is the other thing the PR's page can ask for: an
//! unattended session that addresses the open comments on the PR and on
//! the PRs stacked on it, pushes the changes to all of them at once, and
//! answers the comments. It works on those PRs with a lease on each
//! one's head as it was when the run started, so its push is refused
//! when any of them was pushed to meanwhile. A session that manages the
//! PRs goes on managing them, and hears about the run's push on its
//! next turn. Fixing runs share the reviews' queue and limit, and
//! only one runs in a stack at a time, since each rewrites the PRs above
//! its own.
//!
//! With `RepoSettings::fix_rounds`, a stack of the viewer's PRs gets
//! rounds of both by itself. A round reviews each PR of the stack whose
//! changes weren't reviewed yet, and once those reviews are done, one
//! fixing run addresses what they raised, starting at the lowest PR
//! with such comments. Its push starts the next round. The rounds are
//! counted since the last push that wasn't a fixing run's, and the loop
//! ends early when a round's reviews raise nothing or its fixing run
//! pushes nothing. Each poll works out where a stack stands from its
//! PRs' heads and the `auto_reviews` rows, so nothing else is stored.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use grenadine_core::api::{
    AgentBackend, AgentEventKind, AgentStatus, AutoReview, AutoReviewEntry, AutoReviewState,
    AutoRunKind, MAX_FIX_ROUNDS, NewAgent, PermissionMode, PrDetail, PrKey, RepoSettings,
    ServerEvent, SessionKind,
};
use grenadine_core::feedback::{self, Feedback};

use crate::agent;
use crate::db::Db;
use crate::github::{self, Hit};
use crate::pr_plan::{Above, chain_above};
use crate::sync::State;

/// How many reviews run at once; due PRs beyond that wait for a later
/// poll, and queued reviews for one of the running ones to end.
const MAX_RUNNING: usize = 3;

/// Held while reviews start, so that the poller and the queue don't
/// both start one when only one more may run.
static STARTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long the rows of finished reviews are kept.
const KEEP: i64 = 30 * 24 * 60 * 60;

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

/// What a fixing run's session is told beyond its prompt.
pub const FIX_NOTE: &str = "This is an unattended session that addresses a pull request's review \
    comments. Nobody reads or answers it while it runs, and it's deleted once it's done. Don't \
    ask questions or wait for input. Tools that need approval are denied automatically, so work \
    without them.\n\n\
    For each comment listed in the prompt, decide whether it calls for a change. Don't make \
    changes nobody asked for. A comment that is wrong, or that asks for something that would \
    make the code worse, gets an answer that says why, and no change.\n\n\
    Put each change into the pull request whose comment asked for it: amend that pull \
    request's commits or add a commit on top of them, and rebase the commits of the pull \
    requests above. Check that the code still builds and its tests pass where you can. Then \
    call the grenadine MCP server's update_prs tool once, with one entry per pull request this \
    session manages, bottom first, each naming the pull request's top commit and its number. \
    If no comment called for a change, don't call update_prs.\n\n\
    If update_prs refuses because a pull request's branch moved, someone pushed to it while \
    you worked, and their push wins. Stop there: don't retry, don't rebase onto their commits, \
    don't push any other way, and don't answer the comments. Say what happened and finish.\n\n\
    Once the push went through, or when there was nothing to push, answer on GitHub with \
    `gh`. Reply in every listed thread with what you changed, or why you changed nothing: \
    `gh api repos/OWNER/REPO/pulls/NUMBER/comments/COMMENT_ID/replies -f body=...`. Resolve \
    the threads the prompt marks as yours to resolve, and leave the others for whoever \
    started them: `gh api graphql -f query='mutation { resolveReviewThread(input: {threadId: \
    \"THREAD_ID\"}) { thread { id } } }'`. For each pull request that has review summaries or \
    conversation comments listed, post one short comment on it that answers them, with \
    `gh pr comment NUMBER --body ...`; its first line must be exactly \
    `<!-- grenadine:addressed -->`. Don't change a pull request's title, description or \
    target branch.";

/// What a session of `kind` is told beyond its prompt, when it's
/// unattended.
pub fn note(kind: SessionKind) -> Option<&'static str> {
    match kind {
        SessionKind::Interactive => None,
        SessionKind::AutoReview => Some(NOTE),
        SessionKind::AutoFix => Some(FIX_NOTE),
    }
}

/// The mode a review on `backend` runs in. A fixing run uses it too. An unattended review denies
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
    if new.fix_rounds > MAX_FIX_ROUNDS {
        return Err(format!(
            "{} rounds of review and fixes are too many: at most {MAX_FIX_ROUNDS}",
            new.fix_rounds
        ));
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

/// What a PR's stack's loop says about reviewing the PR's head.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Seen {
    /// An earlier review saw the same changes at another head: the PR
    /// was only rebased since.
    same_changes: bool,
    /// The head is where a fixing run of the loop pushed it, and whether
    /// the loop has rounds left. The next round reviews it whatever the
    /// settings say about new pushes; after the last round nothing does.
    after_fix: Option<bool>,
}

/// Where a stack's loop stands.
#[derive(Clone, Debug, Default, PartialEq)]
struct LoopState {
    /// The rounds done since the last push that wasn't a fixing run's.
    used: u32,
    /// The loop's latest fixing run, when nobody else pushed since.
    last: Option<AutoReviewEntry>,
    /// The loop ran in this stack before.
    ran: bool,
    /// A PR shows the head its latest fixing run found it at, though the
    /// run pushed it elsewhere: GitHub hasn't caught up with the push.
    stale: bool,
}

/// `LoopState` of a stack from the fixing runs that the server started
/// in it, newest first, and its open PRs' heads now.
fn loop_state(fixes: &[AutoReviewEntry], heads: &BTreeMap<u64, String>) -> LoopState {
    let latest = fixes.first();
    // A PR that was merged or closed since doesn't count as pushed to.
    let last = latest.filter(|l| {
        !l.heads.is_empty()
            && l.heads
                .iter()
                .all(|(n, head)| heads.get(n).is_none_or(|now| now == head))
    });
    let stale = latest.is_some_and(|l| {
        heads.iter().any(|(n, now)| {
            l.heads.get(n).is_some_and(|left| left != now) && l.start_heads.get(n) == Some(now)
        })
    });
    LoopState {
        used: last.and_then(|l| l.round).unwrap_or(0),
        last: last.cloned(),
        ran: latest.is_some(),
        stale,
    }
}

impl LoopState {
    /// `Seen::after_fix` of the PR `number` at `head` with `rounds`
    /// rounds allowed.
    fn after_fix(&self, number: u64, head: &str, rounds: u32) -> Option<bool> {
        self.last
            .as_ref()
            .filter(|l| l.state == AutoReviewState::Pushed)
            .filter(|l| l.heads.get(&number).is_some_and(|h| h == head))
            .map(|_| self.used < rounds)
    }

    /// Whether the loop is over until someone else pushes: its rounds
    /// are used up, its last fixing run pushed nothing, or the settings
    /// give a stack only one loop and it had it. It also waits while
    /// GitHub lags behind its last push.
    fn over(&self, settings: &RepoSettings) -> bool {
        self.stale
            || self.used >= settings.fix_rounds
            || self
                .last
                .as_ref()
                .is_some_and(|l| l.state != AutoReviewState::Pushed)
            || (self.ran && self.last.is_none() && !settings.rereview_on_push)
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
    seen: Seen,
) -> bool {
    let wanted = match settings.auto_review {
        AutoReview::Off => false,
        AutoReview::NonDraft => !hit.is_draft,
        AutoReview::IncludingDrafts => true,
    };
    let new = settings
        .auto_review_since
        .is_some_and(|since| created_at >= since);
    // The PR's fixing runs are in the list too.
    let mut reviews = reviews.iter().filter(|r| r.kind == AutoRunKind::Review);
    let at_head = seen.same_changes || reviews.clone().any(|r| r.head_oid == hit.head_oid);
    let reviewed = match seen.after_fix {
        Some(more) => at_head || !more,
        None if settings.rereview_on_push => at_head,
        None => reviews.clone().next().is_some(),
    };
    let busy =
        reviews.any(|r| matches!(r.state, AutoReviewState::Queued | AutoReviewState::Running));
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

/// How many reviews and fixing runs are running.
fn running(db: &Db) -> Result<usize> {
    Ok(db
        .auto_reviews()?
        .iter()
        .filter(|r| r.state == AutoReviewState::Running)
        .count())
}

/// Starts a review of each of `hits` that is due, once it has synced at
/// its head. Failures are logged; the PR gets another try next poll
/// unless its review was recorded as failed. The queued reviews go first.
pub async fn start_due(state: &Arc<State>, hits: &[Hit]) {
    let now = agent::now();
    match state.db.prune_auto_reviews(now - KEEP) {
        Ok(true) => state.send(ServerEvent::AutoReviewsChanged),
        Ok(false) => {}
        Err(e) => tracing::warn!("can't prune the auto-reviews: {e:#}"),
    }
    let _starting = STARTING.lock().await;
    if let Err(e) = start_queued_locked(state).await {
        tracing::warn!("can't start the queued reviews: {e:#}");
    }
    let mut running = match running(&state.db) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("can't list the auto-reviews: {e:#}");
            return;
        }
    };
    let stacks = match stacks(state, hits) {
        Ok(stacks) => stacks,
        Err(e) => {
            tracing::warn!("can't work out the stacks' loops: {e:#}");
            Vec::new()
        }
    };
    let mut settings = BTreeMap::new();
    for hit in hits {
        let key = &hit.key;
        let checked = async {
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
            // A sync that failed or is behind the search waits for the
            // next poll.
            let Some(detail) = state
                .db
                .pr(key)?
                .filter(|d| synced_head(d) == Some(hit.head_oid.as_str()))
            else {
                return Ok(None);
            };
            let seen = seen(state, &stacks, hit, s, &detail, &reviews).await;
            if !due(hit, s, github::unix(&created_at)?, &reviews, running, seen) {
                return Ok(None);
            }
            anyhow::Ok(Some((s.clone(), detail)))
        }
        .await;
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
    match queue_loop_fixes(state, &stacks).await {
        Ok(false) => {}
        Ok(true) => {
            state.send(ServerEvent::AutoReviewsChanged);
            if let Err(e) = start_queued_locked(state).await {
                tracing::warn!("can't start the queued fixing runs: {e:#}");
            }
        }
        Err(e) => tracing::warn!("can't go on with the stacks' loops: {e:#}"),
    }
}

/// The viewer's open PRs of one stack in a repository whose settings ask
/// for rounds of review and fixes, and where the stack's loop stands.
struct LoopStack {
    settings: RepoSettings,
    /// Bottom first, each as synced at the head the search saw.
    prs: Vec<(Hit, PrDetail)>,
    state: LoopState,
}

/// How many PRs of its stack are below `number`.
fn depth(detail: &PrDetail, number: u64) -> usize {
    let Some(stack) = &detail.stack else {
        return 0;
    };
    let parent = |n: u64| {
        stack
            .prs
            .iter()
            .find(|p| p.number == n)
            .and_then(|p| p.parent)
    };
    let (mut depth, mut at) = (0, number);
    while let Some(p) = parent(at) {
        depth += 1;
        at = p;
        if depth > stack.prs.len() {
            break;
        }
    }
    depth
}

/// The stacks among `hits` that get loops. One with a PR that hasn't
/// synced at its head is left out until it has.
fn stacks(state: &State, hits: &[Hit]) -> Result<Vec<LoopStack>> {
    let mut out: Vec<LoopStack> = Vec::new();
    let mut settings = BTreeMap::new();
    for hit in hits {
        let key = &hit.key;
        let s = match settings.get(&key.repo) {
            Some(s) => s,
            None => settings
                .entry(key.repo.clone())
                .or_insert(state.db.repo_settings(&key.repo)?),
        };
        if s.fix_rounds == 0 || s.auto_review == AutoReview::Off {
            continue;
        }
        if out
            .iter()
            .any(|stack| stack.prs.iter().any(|(h, _)| h.key == *key))
        {
            continue;
        }
        let Some(detail) = state.db.pr(key)? else {
            continue;
        };
        let mut prs = Vec::new();
        for other in hits.iter().filter(|h| same_stack(&detail, &h.key)) {
            match state.db.pr(&other.key)? {
                Some(d) if synced_head(&d) == Some(other.head_oid.as_str()) => {
                    prs.push((other.clone(), d))
                }
                _ => {
                    prs.clear();
                    break;
                }
            }
        }
        if prs.is_empty() {
            continue;
        }
        prs.sort_by_key(|(h, d)| (depth(d, h.key.number), h.key.number));
        let mut fixes = Vec::new();
        for (h, _) in &prs {
            fixes.extend(
                state
                    .db
                    .auto_reviews_of(&h.key)?
                    .into_iter()
                    .filter(|r| r.kind == AutoRunKind::Fix && r.round.is_some()),
            );
        }
        fixes.sort_by_key(|r| std::cmp::Reverse((r.started_at, r.id)));
        let heads = prs
            .iter()
            .map(|(h, _)| (h.key.number, h.head_oid.clone()))
            .collect();
        out.push(LoopStack {
            settings: s.clone(),
            prs,
            state: loop_state(&fixes, &heads),
        });
    }
    Ok(out)
}

/// The ids of the changes a PR had at each of its versions, by merge
/// base and head; they don't change, so they're worked out once.
static CHANGES: std::sync::Mutex<BTreeMap<(String, String), String>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Whether a review of `detail`'s PR at another head saw the changes it
/// has now: it was rebased since, and nothing else.
async fn same_changes(state: &State, detail: &PrDetail, reviews: &[AutoReviewEntry]) -> bool {
    let Some(cloned) = state.repos.get(&detail.summary.key.repo) else {
        return false;
    };
    let range = |sha: &str| {
        let v = detail
            .versions
            .iter()
            .find(|v| v.sha == sha && !v.missing)?;
        Some((v.merge_base.clone()?, v.sha.clone()))
    };
    let Some(now) = synced_head(detail).and_then(range) else {
        return false;
    };
    let before: Vec<(String, String)> = reviews
        .iter()
        .filter(|r| r.kind == AutoRunKind::Review && r.head_oid != now.1)
        .filter_map(|r| range(&r.head_oid))
        .collect();
    if before.is_empty() {
        return false;
    }
    let repo = cloned.repo.clone();
    let found = tokio::task::spawn_blocking(move || {
        let id = |range: &(String, String)| -> Option<String> {
            if let Some(id) = CHANGES.lock().unwrap().get(range) {
                return Some(id.clone());
            }
            let id = repo.changes_id(&range.0, &range.1).ok()?;
            CHANGES.lock().unwrap().insert(range.clone(), id.clone());
            Some(id)
        };
        let now = id(&now)?;
        Some(!now.is_empty() && before.iter().any(|b| id(b).as_ref() == Some(&now)))
    })
    .await;
    matches!(found, Ok(Some(true)))
}

/// `Seen` of `hit`, whose PR is `detail` and has the `reviews`.
async fn seen(
    state: &State,
    stacks: &[LoopStack],
    hit: &Hit,
    settings: &RepoSettings,
    detail: &PrDetail,
    reviews: &[AutoReviewEntry],
) -> Seen {
    let after_fix = stacks
        .iter()
        .find(|stack| stack.prs.iter().any(|(h, _)| h.key == hit.key))
        .and_then(|stack| {
            stack
                .state
                .after_fix(hit.key.number, &hit.head_oid, settings.fix_rounds)
        });
    // Only a head that no review saw can be a rebase of one that did.
    let at_head = reviews
        .iter()
        .any(|r| r.kind == AutoRunKind::Review && r.head_oid == hit.head_oid);
    let rereviews = settings.rereview_on_push || after_fix.is_some();
    Seen {
        same_changes: rereviews && !at_head && same_changes(state, detail, reviews).await,
        after_fix,
    }
}

/// What a fixing run of `detail`'s PR addresses there: everything open
/// when it was asked for, and only what's posted under the viewer's
/// name, such as an auto-review's comments, when the loop started it.
fn to_address(detail: &PrDetail, asked_for: bool) -> Feedback<'_> {
    let open = feedback::open(detail);
    match &detail.review {
        Some(review) if !asked_for => open.by(&review.viewer),
        _ => open,
    }
}

/// Queues the next fixing run of each of `stacks` whose round's reviews
/// are done and raised something. Returns whether it queued any.
async fn queue_loop_fixes(state: &Arc<State>, stacks: &[LoopStack]) -> Result<bool> {
    let mut queued = false;
    for stack in stacks {
        let s = &stack.settings;
        if stack.state.over(s) {
            continue;
        }
        let last = stack.state.last.as_ref();
        // The round's reviews: none on its way or still to start, and
        // one that came after the last fixing run.
        let (mut waiting, mut reviewed) = (false, false);
        for (hit, detail) in &stack.prs {
            let rows = state.db.auto_reviews_of(&hit.key)?;
            waiting |= rows
                .iter()
                .any(|r| matches!(r.state, AutoReviewState::Queued | AutoReviewState::Running));
            let Some(created_at) = state.db.pr_created_at(&hit.key)? else {
                waiting = true;
                continue;
            };
            let seen = Seen {
                same_changes: same_changes(state, detail, &rows).await,
                after_fix: stack
                    .state
                    .after_fix(hit.key.number, &hit.head_oid, s.fix_rounds),
            };
            // Whatever the limit: one that waits for room is still due.
            waiting |= due(hit, s, github::unix(&created_at)?, &rows, 0, seen);
            reviewed |= rows.iter().any(|r| {
                r.kind == AutoRunKind::Review
                    && r.state == AutoReviewState::Posted
                    && (r.head_oid == hit.head_oid || seen.same_changes)
                    && last.is_none_or(|l| r.started_at >= l.finished_at.unwrap_or(l.started_at))
            });
        }
        if waiting || !reviewed {
            continue;
        }
        let Some((hit, detail)) = stack
            .prs
            .iter()
            .find(|(_, d)| !to_address(d, false).is_empty())
        else {
            continue;
        };
        // A stack that forks or has someone else's PR above gets none.
        if fix_chain(&state.db, detail)?.is_err() {
            continue;
        }
        let round = stack.state.used + 1;
        let id = state.db.queue_loop_fix(
            &hit.key,
            &hit.head_oid,
            &detail.summary.title,
            &detail.summary.url,
            round,
            agent::now(),
        )?;
        if id.is_some() {
            tracing::info!(
                "{}#{}: round {round} of {}: queued a fixing run",
                hit.key.repo,
                hit.key.number,
                s.fix_rounds
            );
            queued = true;
        }
    }
    Ok(queued)
}

/// Why a review can't be asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The PR hasn't synced, or has no commits.
    NotSynced,
    NotOpen,
    /// Someone else opened it.
    NotYours,
    /// It already has a review queued or running, or a fixing run when
    /// that was asked for.
    Busy,
    /// Several open PRs are stacked on `at`, the PR or one above it, so a
    /// fixing run can't rebase the PRs above along with it.
    Forked {
        at: u64,
    },
    /// Neither the PR nor the ones stacked on it have open comments.
    NothingOpen,
    /// Someone else's PR is stacked on it, which a fixing run would
    /// rewrite.
    NotYoursAbove {
        number: u64,
    },
    /// The repository's review backend isn't offered; its label.
    NotOffered(&'static str),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NotSynced => write!(f, "the PR hasn't synced yet"),
            Refused::NotOpen => write!(f, "the PR isn't open"),
            Refused::NotYours => write!(f, "only your own PRs can be auto-reviewed"),
            Refused::Busy => write!(f, "one is already queued or running for this PR"),
            Refused::Forked { at } => write!(
                f,
                "several pull requests are stacked on #{at}, and a fixing run can only rebase \
                 one chain of them"
            ),
            Refused::NotYoursAbove { number } => write!(
                f,
                "#{number}, which is stacked on it, isn't yours, and a fixing run would rewrite it"
            ),
            Refused::NothingOpen => write!(
                f,
                "neither this PR nor the ones stacked on it have open comments"
            ),
            Refused::NotOffered(backend) => write!(f, "{backend} isn't available"),
        }
    }
}

/// The PRs a fixing run started from `detail`'s PR covers, bottom
/// first: the PR and the open ones stacked on it, as far as they've
/// synced.
fn fix_chain(db: &Db, detail: &PrDetail) -> Result<std::result::Result<Vec<PrDetail>, Refused>> {
    let number = detail.summary.key.number;
    let above = match detail.stack.as_ref().map(|s| chain_above(s, number)) {
        Some(Above::Fork { at, .. }) => return Ok(Err(Refused::Forked { at })),
        Some(Above::Linear(above)) => above,
        None => Vec::new(),
    };
    let mut chain = vec![detail.clone()];
    for number in above {
        let key = PrKey {
            repo: detail.summary.key.repo.clone(),
            number,
        };
        match db.pr(&key)? {
            Some(d) if d.summary.author != detail.summary.author => {
                return Ok(Err(Refused::NotYoursAbove { number }));
            }
            Some(d) if synced_head(&d).is_some() => chain.push(d),
            _ => return Ok(Err(Refused::NotSynced)),
        }
    }
    Ok(Ok(chain))
}

/// Whether the PRs `a` and `b` are in one stack, going by `a`'s.
fn same_stack(a: &PrDetail, b: &PrKey) -> bool {
    let key = &a.summary.key;
    key.repo == b.repo
        && (key.number == b.number
            || a.stack
                .as_ref()
                .is_some_and(|s| s.prs.iter().any(|p| p.number == b.number)))
}

/// Queues a review or fixing run of `key`, asked for from its page, and
/// starts it if fewer than `MAX_RUNNING` run. Returns its id.
pub async fn ask(
    state: &Arc<State>,
    key: &PrKey,
    kind: AutoRunKind,
) -> Result<std::result::Result<i64, Refused>> {
    let Some(detail) = state.db.pr(key)? else {
        return Ok(Err(Refused::NotSynced));
    };
    let Some(head) = synced_head(&detail) else {
        return Ok(Err(Refused::NotSynced));
    };
    if detail.summary.state != "OPEN" {
        return Ok(Err(Refused::NotOpen));
    }
    let Some(review) = &detail.review else {
        return Ok(Err(Refused::NotSynced));
    };
    if review.viewer != detail.summary.author {
        return Ok(Err(Refused::NotYours));
    }
    let backend = state.db.repo_settings(&key.repo)?.review_backend;
    if !state.agents.offered(backend).await {
        return Ok(Err(Refused::NotOffered(backend.label())));
    }
    if kind == AutoRunKind::Fix {
        match fix_chain(&state.db, &detail)? {
            Ok(chain) if chain.iter().all(|d| feedback::open(d).is_empty()) => {
                return Ok(Err(Refused::NothingOpen));
            }
            Ok(_) => {}
            Err(why) => return Ok(Err(why)),
        }
    }
    let queued = state.db.queue_auto_review(
        key,
        kind,
        head,
        &detail.summary.title,
        &detail.summary.url,
        agent::now(),
    )?;
    let Some(id) = queued else {
        return Ok(Err(Refused::Busy));
    };
    tracing::info!(
        "{}#{}: a {} was asked for",
        key.repo,
        key.number,
        kind.as_str()
    );
    state.send(ServerEvent::AutoReviewsChanged);
    start_queued(state).await;
    Ok(Ok(id))
}

/// Cancels the review `id`: a queued one doesn't start, and a running
/// one's session is deleted. Returns whether it was queued or running.
pub async fn cancel(state: &Arc<State>, id: i64) -> Result<bool> {
    let Some(review) = state.db.auto_review(id)? else {
        return Ok(false);
    };
    match (review.state, review.session) {
        (AutoReviewState::Queued, _) => {
            let cancelled = state.db.end_queued_auto_review(
                id,
                AutoReviewState::Cancelled,
                None,
                agent::now(),
            )?;
            if cancelled {
                state.send(ServerEvent::AutoReviewsChanged);
                return Ok(true);
            }
            // It started meanwhile.
            match state.db.auto_review(id)?.and_then(|r| r.session) {
                Some(session) => agent::delete(state, &session).await,
                None => Ok(false),
            }
        }
        (AutoReviewState::Running, Some(session)) => agent::delete(state, &session).await,
        _ => Ok(false),
    }
}

/// Starts the queued reviews in the background, as far as fewer than
/// `MAX_RUNNING` run.
pub fn kick(state: &Arc<State>) {
    let state = state.clone();
    tokio::spawn(async move { start_queued(&state).await });
}

/// Starts the queued reviews, oldest first, while fewer than
/// `MAX_RUNNING` run.
pub async fn start_queued(state: &Arc<State>) {
    let _starting = STARTING.lock().await;
    if let Err(e) = start_queued_locked(state).await {
        tracing::warn!("can't start the queued reviews: {e:#}");
    }
}

async fn start_queued_locked(state: &Arc<State>) -> Result<()> {
    let mut running = running(&state.db)?;
    // The PRs with a fixing run going: another one in the same stack
    // would rewrite the same PRs.
    let mut fixing: Vec<PrKey> = state
        .db
        .auto_reviews()?
        .into_iter()
        .filter(|r| r.kind == AutoRunKind::Fix && r.state == AutoReviewState::Running)
        .map(|r| r.key)
        .collect();
    for review in state.db.queued_auto_reviews()? {
        if running >= MAX_RUNNING {
            break;
        }
        let key = &review.key;
        let Some(detail) = state.db.pr(key)? else {
            continue;
        };
        let cancel = |why: &str| -> Result<()> {
            let ended = state.db.end_queued_auto_review(
                review.id,
                AutoReviewState::Cancelled,
                Some(why),
                agent::now(),
            )?;
            if ended {
                state.send(ServerEvent::AutoReviewsChanged);
            }
            Ok(())
        };
        if detail.summary.state != "OPEN" {
            cancel("The PR was closed before it started.")?;
            continue;
        }
        let Some(head) = synced_head(&detail) else {
            continue;
        };
        let mut chain = Vec::new();
        if review.kind == AutoRunKind::Fix {
            if fixing.iter().any(|k| same_stack(&detail, k)) {
                continue;
            }
            match fix_chain(&state.db, &detail)? {
                Ok(prs) if prs.iter().all(|d| to_address(d, review.manual).is_empty()) => {
                    cancel("No open comments were left when it could start.")?;
                    continue;
                }
                Ok(prs) => chain = prs,
                Err(Refused::NotSynced) => continue,
                Err(why) => {
                    cancel(&format!("It couldn't start: {why}."))?;
                    continue;
                }
            }
        }
        let settings = state.db.repo_settings(&key.repo)?;
        if !state.agents.offered(settings.review_backend).await {
            continue;
        }
        let begun = state.db.begin_queued_auto_review(
            review.id,
            head,
            &detail.summary.title,
            &detail.summary.url,
            agent::now(),
        )?;
        let kind = review.kind;
        if begun && kind == AutoRunKind::Fix {
            // Where the run found its PRs, to tell a later push by
            // someone else from its own.
            let heads = chain
                .iter()
                .filter_map(|d| Some((d.summary.key.number, synced_head(d)?.to_owned())))
                .collect();
            state.db.add_auto_fix_heads(review.id, &heads, true)?;
        }
        let asked_for = review.manual;
        if begun
            && launch(
                state, review.id, kind, asked_for, head, &settings, &detail, &chain,
            )
            .await
        {
            running += 1;
            if kind == AutoRunKind::Fix {
                fixing.push(key.clone());
            }
        }
    }
    Ok(())
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
    let kind = AutoRunKind::Review;
    launch(state, id, kind, false, &hit.head_oid, settings, detail, &[]).await
}

/// Starts the session of the review or fixing run `id` of `detail`'s PR
/// at `head`, which is recorded as running. `chain` is what a fixing run
/// covers, as `fix_chain` has it, and `asked_for` whether it was asked
/// for from the PR's page. Returns whether it's running.
#[allow(clippy::too_many_arguments)]
async fn launch(
    state: &Arc<State>,
    id: i64,
    kind: AutoRunKind,
    asked_for: bool,
    head: &str,
    settings: &RepoSettings,
    detail: &PrDetail,
    chain: &[PrDetail],
) -> bool {
    let key = &detail.summary.key;
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
        "{}#{}: starting a {} of {head}",
        key.repo,
        key.number,
        kind.as_str()
    );
    let (prompt, session_kind, title) = match kind {
        AutoRunKind::Review => (
            prompt(settings.review_skill.as_deref(), detail),
            SessionKind::AutoReview,
            format!("Review #{}: {}", key.number, detail.summary.title),
        ),
        AutoRunKind::Fix => (
            fix_prompt(chain, asked_for),
            SessionKind::AutoFix,
            format!(
                "Address comments on #{}: {}",
                key.number, detail.summary.title
            ),
        ),
    };
    let req = NewAgent {
        backend: settings.review_backend,
        repo: key.repo.clone(),
        pr: Some(key.number),
        prompt,
        permission_mode: review_mode(settings.review_backend),
        model: model.cloned(),
        images: Vec::new(),
        draft: None,
    };
    let (running, recorded) =
        match agent::create_session(state, req, session_kind, Some(title), None).await {
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

/// `text` as a Markdown quote, cut to a length that keeps a prompt with
/// many comments readable.
fn quoted(text: &str) -> String {
    const MAX: usize = 2000;
    let text = text.trim();
    let cut = match text.char_indices().nth(MAX) {
        Some((at, _)) => format!("{} […]", &text[..at]),
        None => text.to_owned(),
    };
    cut.lines()
        .map(|l| format!("  > {l}").trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The open feedback `open` on `detail`'s PR as a section of a fixing
/// run's prompt.
fn feedback_lines(detail: &PrDetail, open: &Feedback) -> String {
    let pr = &detail.summary;
    let viewer = detail.review.as_ref().map(|r| r.viewer.as_str());
    let mut out = format!("## #{} {} ({})\n", pr.key.number, pr.title, pr.url);
    for t in &open.threads {
        let root = t.root;
        let at = match root.line.or(root.original_line) {
            Some(line) if !root.on_file => format!("{}:{line}", root.path),
            _ => root.path.clone(),
        };
        let outdated = match root.line {
            None if !root.on_file => ", on an older version of the code",
            _ => "",
        };
        let resolve = match (&root.thread_id, Some(root.author.as_str()) == viewer) {
            (Some(id), true) => format!("yours to resolve, thread id {id}"),
            _ => "leave it unresolved".to_owned(),
        };
        out += &format!(
            "\n- Thread on {at}{outdated} (comment id {}; {resolve}):\n  @{}:\n{}\n",
            root.id,
            root.author,
            quoted(&root.body)
        );
        for reply in &t.replies {
            out += &format!("  @{} replied:\n{}\n", reply.author, quoted(&reply.body));
        }
    }
    let login =
        |p: &grenadine_core::api::Person| p.login.clone().unwrap_or_else(|| "someone".into());
    for r in &open.summaries {
        out += &format!(
            "\n- Review summary by @{} ({}):\n{}\n",
            login(&r.author),
            r.url,
            quoted(&r.body)
        );
    }
    for c in &open.comments {
        out += &format!(
            "\n- Conversation comment by @{} ({}):\n{}\n",
            login(&c.author),
            c.url,
            quoted(&c.body)
        );
    }
    out
}

/// The prompt of a fixing run that covers `chain`, bottom first: the
/// feedback on each of its PRs that it's to address, as `to_address`
/// has it.
fn fix_prompt(chain: &[PrDetail], asked_for: bool) -> String {
    let Some(first) = chain.first() else {
        return String::new();
    };
    let pr = &first.summary;
    let mut out = format!(
        "Address the open review comments on pull request {}: \"{}\" ({}#{})",
        pr.url, pr.title, pr.key.repo, pr.key.number
    );
    out += match chain.len() {
        1 => ".",
        _ => " and on the pull requests stacked on it, which this session manages too.",
    };
    out += " These are the comments, bottom pull request first:\n";
    for detail in chain {
        let open = to_address(detail, asked_for);
        if !open.is_empty() {
            out += &format!("\n{}", feedback_lines(detail, &open));
        }
    }
    out
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
        Ok(Some(s)) if s.kind.unattended() => {}
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
        // Its slot is free for a queued review.
        start_queued(&state).await;
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
    let fix = review.kind == AutoRunKind::Fix;
    match (last, fix) {
        (AgentStatus::Idle, _) if answered => {}
        (AgentStatus::Idle | AgentStatus::Interrupted, false) => {
            return failed("The review was interrupted.");
        }
        (AgentStatus::Idle | AgentStatus::Interrupted, true) => {
            return failed("The fixing run was interrupted.");
        }
        (_, false) => return failed("The review's session failed."),
        (_, true) => return failed("The fixing run's session failed."),
    }
    if fix {
        return finish_fix(state, id, review).await;
    }
    for &delay in &state.posted_checks {
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

/// Records where the fixing run of session `id` left PRs, `heads` by
/// PR number: where it pushed them, or with `found`, where it found
/// ones that joined it. A head recorded here isn't taken for someone
/// else's push later.
pub fn record_heads(
    state: &State,
    id: &str,
    heads: &BTreeMap<u64, String>,
    found: bool,
) -> Result<()> {
    if let Some(run) = state.db.auto_review_of(id)? {
        state.db.add_auto_fix_heads(run.id, heads, found)?;
    }
    Ok(())
}

/// How a fixing run that didn't push ended, going by its PRs as they are
/// now, each with the head it had when the run started at `started_at`:
/// superseded when one of them moved, posted when the viewer answered on
/// one of them since. `None` when neither shows yet.
fn unpushed_outcome(
    prs: &[(Option<String>, PrDetail)],
    started_at: i64,
) -> Option<AutoReviewState> {
    let moved = prs
        .iter()
        .any(|(lease, detail)| lease.is_some() && synced_head(detail) != lease.as_deref());
    let answered = prs
        .iter()
        .any(|(_, detail)| posted_since(detail, started_at));
    match (moved, answered) {
        (true, _) => Some(AutoReviewState::Superseded),
        (false, true) => Some(AutoReviewState::Posted),
        (false, false) => None,
    }
}

/// Ends the fixing run `review`, whose session `id` finished its turn:
/// as pushed when it landed its commits, as superseded when one of its
/// PRs was pushed to since it started, and as posted when it only
/// answered the comments. The session of any of those is deleted. One
/// that did none of them is kept.
async fn finish_fix(state: &Arc<State>, id: &str, review: &AutoReviewEntry) -> Result<()> {
    let end = |as_: AutoReviewState| {
        state
            .db
            .finish_auto_review(review.id, as_, None, agent::now())
    };
    let pushed = state
        .db
        .agent_events(id)?
        .iter()
        .any(|e| matches!(e.kind, AgentEventKind::Landed(_)));
    if pushed {
        end(AutoReviewState::Pushed)?;
        tracing::info!(
            "{}#{}: the fixes are pushed",
            review.key.repo,
            review.key.number
        );
        agent::delete(state, id).await?;
        return Ok(());
    }
    // The PRs it took when it started, each with the head it had then.
    let taken: Vec<(PrKey, Option<String>)> = state
        .db
        .agent(id)?
        .map(|s| {
            agent::landed_prs(&s)
                .into_iter()
                .map(|pr| {
                    let key = PrKey {
                        repo: review.key.repo.clone(),
                        number: pr.number,
                    };
                    (key, pr.lease)
                })
                .collect()
        })
        .unwrap_or_default();
    for &delay in &state.posted_checks {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = state.shutdown.cancelled() => return Ok(()),
        }
        let mut now = Vec::new();
        for (key, lease) in &taken {
            if state.resync_now(key).await
                && let Some(detail) = state.db.pr(key)?
            {
                now.push((lease.clone(), detail));
            }
        }
        if let Some(as_) = unpushed_outcome(&now, review.started_at) {
            end(as_)?;
            agent::delete(state, id).await?;
            return Ok(());
        }
    }
    end(AutoReviewState::Unposted)
}

#[cfg(test)]
mod tests {
    use grenadine_core::api::{
        CostUnit, IssueComment, MergeInfo, MergeState, Person, PrKey, PrSummary, Review,
        ReviewComment, ReviewInfo, ReviewState, Side, Stack, StackPr, ThreadCounts, Version,
        VersionKind,
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
            queued: false,
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
            kind: AutoRunKind::Review,
            key: PrKey {
                repo: "o/n".into(),
                number: 2,
            },
            title: "t".into(),
            url: "u".into(),
            head_oid: head.into(),
            state,
            manual: false,
            session: None,
            status: None,
            started_at: 0,
            finished_at: None,
            cost: 0.0,
            cost_unit: CostUnit::Usd,
            error: None,
            round: None,
            heads: Default::default(),
            start_heads: Default::default(),
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
        assert!(due(&hit(false, "a"), &s, 100, &[], 0, Seen::default()));
        // Opened before reviews started, a draft, or too many running.
        assert!(!due(&hit(false, "a"), &s, 99, &[], 0, Seen::default()));
        assert!(!due(&hit(true, "a"), &s, 100, &[], 0, Seen::default()));
        assert!(!due(
            &hit(false, "a"),
            &s,
            100,
            &[],
            MAX_RUNNING,
            Seen::default()
        ));
        let s = settings(AutoReview::IncludingDrafts, false);
        assert!(due(&hit(true, "a"), &s, 100, &[], 0, Seen::default()));
        let s = settings(AutoReview::Off, false);
        assert!(!due(&hit(false, "a"), &s, 100, &[], 0, Seen::default()));
        let s = RepoSettings {
            auto_review_since: None,
            ..settings(AutoReview::NonDraft, false)
        };
        assert!(!due(&hit(false, "a"), &s, 100, &[], 0, Seen::default()));

        // Without re-reviews, a PR is reviewed once.
        let s = settings(AutoReview::NonDraft, false);
        let done = [entry("a", AutoReviewState::Posted)];
        assert!(!due(&hit(false, "b"), &s, 100, &done, 0, Seen::default()));
        // With them, each head is, but not while a review is running.
        let s = settings(AutoReview::NonDraft, true);
        assert!(!due(&hit(false, "a"), &s, 100, &done, 0, Seen::default()));
        assert!(due(&hit(false, "b"), &s, 100, &done, 0, Seen::default()));
        let running = [entry("a", AutoReviewState::Running)];
        assert!(!due(
            &hit(false, "b"),
            &s,
            100,
            &running,
            0,
            Seen::default()
        ));
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

        let rounds = |n| RepoSettings {
            fix_rounds: n,
            ..off.clone()
        };
        let saved = to_save(&off, rounds(MAX_FIX_ROUNDS), 0).unwrap();
        assert_eq!(saved.fix_rounds, MAX_FIX_ROUNDS);
        assert!(to_save(&off, rounds(MAX_FIX_ROUNDS + 1), 0).is_err());
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
            queued: false,
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
                queued: false,
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
        let session = agent::create_session(&state, req, SessionKind::AutoReview, title, None)
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
        let session = agent::create_session(&state, req, SessionKind::AutoReview, title, None)
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
                    queued: false,
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
    fn a_pr_has_one_review_queued_or_running() {
        let db = Db::in_memory();
        let key = PrKey {
            repo: "o/n".into(),
            number: 2,
        };
        let other = PrKey {
            repo: "o/n".into(),
            number: 3,
        };
        let id = db
            .start_auto_review(&key, "a", "t", "u", 10)
            .unwrap()
            .unwrap();
        // Not while one runs, whichever commit.
        assert_eq!(db.start_auto_review(&key, "b", "t", "u", 11).unwrap(), None);
        assert_eq!(
            db.queue_auto_review(&key, AutoRunKind::Review, "b", "t", "u", 11)
                .unwrap(),
            None
        );
        assert!(
            db.queue_auto_review(&other, AutoRunKind::Review, "c", "t", "u", 11)
                .unwrap()
                .is_some()
        );
        // Once it ended, the same commit can be reviewed again.
        db.finish_auto_review(id, AutoReviewState::Posted, None, 12)
            .unwrap();
        let queued = db
            .queue_auto_review(&key, AutoRunKind::Review, "a", "t", "u", 13)
            .unwrap()
            .unwrap();
        assert_eq!(db.start_auto_review(&key, "a", "t", "u", 14).unwrap(), None);
        let all = db.auto_reviews_of(&key).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].manual && !all[1].manual);
        assert_eq!(all[0].state, AutoReviewState::Queued);

        // The queue is oldest first, and a queued review starts at the
        // commit it's given.
        let order: Vec<_> = db
            .queued_auto_reviews()
            .unwrap()
            .into_iter()
            .map(|r| r.key.number)
            .collect();
        assert_eq!(order, [3, 2]);
        assert!(
            db.begin_queued_auto_review(queued, "z", "T", "U", 15)
                .unwrap()
        );
        assert!(
            !db.begin_queued_auto_review(queued, "z", "T", "U", 15)
                .unwrap()
        );
        let r = db.auto_review(queued).unwrap().unwrap();
        assert_eq!(
            (r.state, r.head_oid.as_str(), r.title.as_str(), r.started_at),
            (AutoReviewState::Running, "z", "T", 15)
        );

        // A server that stopped during a review failed it, but leaves the
        // queue; ending it again changes nothing.
        db.interrupt_auto_reviews(20).unwrap();
        db.finish_auto_review(queued, AutoReviewState::Posted, None, 30)
            .unwrap();
        let r = db.auto_review(queued).unwrap().unwrap();
        assert_eq!(
            (r.state, r.finished_at),
            (AutoReviewState::Failed, Some(20))
        );
        let left = db.queued_auto_reviews().unwrap();
        assert_eq!(left.len(), 1);
        assert!(
            db.end_queued_auto_review(left[0].id, AutoReviewState::Cancelled, None, 21)
                .unwrap()
        );
        assert!(
            !db.end_queued_auto_review(left[0].id, AutoReviewState::Cancelled, None, 21)
                .unwrap()
        );

        // Old rows without a session go; a queued one isn't old.
        db.queue_auto_review(&other, AutoRunKind::Review, "d", "t", "u", 0)
            .unwrap()
            .unwrap();
        assert!(db.prune_auto_reviews(25).unwrap());
        let kept = db.auto_reviews().unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].state, AutoReviewState::Queued);
    }

    fn review_info(viewer: &str) -> ReviewInfo {
        ReviewInfo {
            viewer: viewer.into(),
            decision: None,
            reviews: vec![],
            requested: vec![],
            can_edit_reviewers: false,
            comments: vec![],
            checks: vec![],
            ci: None,
            pending_review: None,
            can_update: true,
            merge: MergeInfo {
                conflicting: None,
                state: MergeState::Clean,
                methods: vec![],
                default_method: None,
                delete_branch_on_merge: false,
                can_merge_as_admin: false,
                requires_conversation_resolution: false,
                queue: None,
                stack: None,
            },
        }
    }

    /// Stores PR `number` of the fixture's repository as synced at
    /// `head`, opened by `author`, `pr_state`, with the viewer "me".
    fn store_pr(state: &State, slug: &str, number: u64, head: &str, author: &str, pr_state: &str) {
        store_full(state, slug, number, head, author, pr_state, &[], None);
    }

    /// `store_pr` of a PR with the inline `comments`, in `stack`.
    #[allow(clippy::too_many_arguments)]
    fn store_full(
        state: &State,
        slug: &str,
        number: u64,
        head: &str,
        author: &str,
        pr_state: &str,
        comments: &[ReviewComment],
        stack: Option<&Stack>,
    ) {
        state
            .db
            .store_sync(
                &crate::db::PrMeta {
                    key: PrKey {
                        repo: slug.into(),
                        number,
                    },
                    title: format!("PR {number}"),
                    body: String::new(),
                    author: author.into(),
                    state: pr_state.into(),
                    is_draft: false,
                    queued: false,
                    url: format!("u{number}"),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    base_ref: "main".into(),
                    head_ref: "b".into(),
                    head_oid: head.into(),
                    ci: None,
                    review: Some(review_info("me")),
                    review_error: None,
                },
                &[Version {
                    number: 1,
                    sha: head.into(),
                    merge_base: None,
                    kind: VersionKind::Initial,
                    pushed_at: None,
                    pushed_by: None,
                    pushed_by_is_guess: false,
                    missing: false,
                }],
                comments,
                stack,
                false,
                None,
            )
            .unwrap();
    }

    /// An unresolved inline thread's first comment.
    fn thread(id: u64, author: &str) -> ReviewComment {
        ReviewComment {
            id,
            in_reply_to: None,
            author: author.into(),
            body: format!("Fix thing {id}.\nPlease."),
            path: "src/a.rs".into(),
            original_commit: "c".into(),
            original_line: Some(7),
            original_start_line: None,
            line: Some(9),
            start_line: None,
            start_side: None,
            side: Side::Right,
            on_file: false,
            created_at: "2026-01-01T00:00:00Z".into(),
            url: String::new(),
            resolved: false,
            resolved_by: None,
            thread_id: Some(format!("T{id}")),
            can_resolve: true,
            pending: false,
            node_id: None,
        }
    }

    fn stack_of(prs: Vec<StackPr>) -> Stack {
        Stack {
            base_ref: "main".into(),
            prs,
            more_ancestors: false,
            more_descendants: false,
        }
    }

    /// A fixing run's prompt lists the open feedback of each PR it
    /// covers, and says which threads are the viewer's to resolve.
    #[test]
    fn fix_prompts() {
        let mut lower = detail(7, None);
        lower.review = Some(review_info("me"));
        let mut reply = thread(12, "ann");
        reply.in_reply_to = Some(11);
        reply.body = "Also this.".into();
        let mut outdated = thread(13, "me");
        outdated.line = None;
        lower.comments = vec![thread(11, "ann"), reply, outdated];
        let mut upper = detail(8, None);
        upper.review = Some(ReviewInfo {
            reviews: vec![Review {
                author: person("ann"),
                state: ReviewState::Commented,
                body: "Rename it.".into(),
                submitted_at: "2026-01-01T00:00:00Z".into(),
                url: "r1".into(),
                commit: None,
            }],
            comments: vec![IssueComment {
                author: person("bob"),
                body: "Add a test.".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                url: "c1".into(),
            }],
            ..review_info("me")
        });
        // A PR of the chain without open feedback isn't listed.
        let mut quiet = detail(9, None);
        quiet.review = Some(review_info("me"));

        let prompt = fix_prompt(&[lower.clone(), upper, quiet], true);
        assert_eq!(
            prompt,
            "Address the open review comments on pull request https://github.com/o/n/pull/7: \
             \"PR 7\" (o/n#7) and on the pull requests stacked on it, which this session \
             manages too. These are the comments, bottom pull request first:\n\
             \n\
             ## #7 PR 7 (https://github.com/o/n/pull/7)\n\
             \n\
             - Thread on src/a.rs:9 (comment id 11; leave it unresolved):\n  @ann:\n  \
             > Fix thing 11.\n  > Please.\n  @ann replied:\n  > Also this.\n\
             \n\
             - Thread on src/a.rs:7, on an older version of the code (comment id 13; yours to \
             resolve, thread id T13):\n  @me:\n  > Fix thing 13.\n  > Please.\n\
             \n\
             ## #8 PR 8 (https://github.com/o/n/pull/8)\n\
             \n\
             - Review summary by @ann (r1):\n  > Rename it.\n\
             \n\
             - Conversation comment by @bob (c1):\n  > Add a test.\n"
        );
        assert!(fix_prompt(&[lower], true).starts_with(
            "Address the open review comments on pull request https://github.com/o/n/pull/7: \
             \"PR 7\" (o/n#7). These are"
        ));
        // A long comment is cut.
        assert!(quoted(&"x".repeat(5000)).ends_with("x […]"));
    }

    /// A fixing run that didn't push was superseded when one of its PRs
    /// moved, and otherwise counts as posted once the viewer answered.
    #[test]
    fn how_an_unpushed_fix_ended() {
        let at = |head: &str| {
            let mut d = detail(7, None);
            d.review = Some(review_info("me"));
            d.versions = vec![Version {
                number: 1,
                sha: head.into(),
                merge_base: None,
                kind: VersionKind::Initial,
                pushed_at: None,
                pushed_by: None,
                pushed_by_is_guess: false,
                missing: false,
            }];
            d
        };
        let lease = |head: &str| Some(head.to_owned());
        assert_eq!(unpushed_outcome(&[(lease("a"), at("a"))], 0), None);
        assert_eq!(unpushed_outcome(&[], 0), None);
        let mut answered = at("a");
        let mut reply = thread(1, "me");
        reply.created_at = "2026-01-02T00:00:00Z".into();
        answered.comments = vec![reply];
        let since = github::unix("2026-01-01T12:00:00Z").unwrap();
        assert_eq!(
            unpushed_outcome(
                &[(lease("a"), at("a")), (lease("a"), answered.clone())],
                since
            ),
            Some(AutoReviewState::Posted)
        );
        // Someone's push wins over the answers.
        assert_eq!(
            unpushed_outcome(&[(lease("a"), at("z")), (lease("a"), answered)], since),
            Some(AutoReviewState::Superseded)
        );
    }

    const STALL: &str = "#!/bin/sh
sleep 30
";

    /// A review asked for from the PR's page starts right away when
    /// there's room, and the PR gets no other until it ends.
    #[tokio::test]
    async fn asking_for_a_review() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(&fx, dir.path(), STALL);
        let slug = fx.clone.slug.clone();
        let head = fx.clone.head().unwrap();
        let key = PrKey {
            repo: slug.clone(),
            number: 2,
        };
        store_pr(&state, &slug, 2, &head, "me", "OPEN");
        // Settings that don't review the repository's PRs by themselves
        // don't keep one from being asked for.
        state
            .db
            .set_repo_settings(&slug, &settings(AutoReview::Off, false))
            .unwrap();

        let id = ask(&state, &key, AutoRunKind::Review)
            .await
            .unwrap()
            .unwrap();
        let review = state.db.auto_review(id).unwrap().unwrap();
        assert_eq!(review.state, AutoReviewState::Running);
        assert!(review.manual);
        assert_eq!(review.head_oid, head);
        let session = review.session.clone().unwrap();
        assert_eq!(
            state.db.agent(&session).unwrap().unwrap().kind,
            SessionKind::AutoReview
        );
        assert_eq!(
            ask(&state, &key, AutoRunKind::Review).await.unwrap(),
            Err(Refused::Busy)
        );

        // Cancelling it deletes its session; then the same commit can be
        // reviewed again.
        assert!(cancel(&state, id).await.unwrap());
        assert!(state.db.agent(&session).unwrap().is_none());
        let review = state.db.auto_review(id).unwrap().unwrap();
        assert_eq!(review.state, AutoReviewState::Cancelled);
        assert!(!cancel(&state, id).await.unwrap());
        let again = ask(&state, &key, AutoRunKind::Review)
            .await
            .unwrap()
            .unwrap();
        let session = state
            .db
            .auto_review(again)
            .unwrap()
            .unwrap()
            .session
            .unwrap();
        agent::delete(&state, &session).await.unwrap();
    }

    #[tokio::test]
    async fn what_cant_be_reviewed() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(&fx, dir.path(), STALL);
        let slug = fx.clone.slug.clone();
        let head = fx.clone.head().unwrap();
        let pr = |number| PrKey {
            repo: slug.clone(),
            number,
        };
        store_pr(&state, &slug, 2, &head, "other", "OPEN");
        store_pr(&state, &slug, 3, &head, "me", "MERGED");
        assert_eq!(
            ask(&state, &pr(2), AutoRunKind::Review).await.unwrap(),
            Err(Refused::NotYours)
        );
        assert_eq!(
            ask(&state, &pr(3), AutoRunKind::Review).await.unwrap(),
            Err(Refused::NotOpen)
        );
        assert_eq!(
            ask(&state, &pr(4), AutoRunKind::Review).await.unwrap(),
            Err(Refused::NotSynced)
        );
        assert!(state.db.auto_reviews().unwrap().is_empty());

        // Nor while the review's backend isn't offered.
        let dir = tempfile::tempdir().unwrap();
        let state =
            crate::agent::devin_tests::state_with_devin(&fx, dir.path(), dir.path().join("devin"));
        store_pr(&state, &slug, 2, &head, "me", "OPEN");
        state
            .db
            .set_repo_settings(
                &slug,
                &RepoSettings {
                    review_backend: AgentBackend::Devin,
                    ..RepoSettings::default()
                },
            )
            .unwrap();
        assert_eq!(
            ask(&state, &pr(2), AutoRunKind::Review).await.unwrap(),
            Err(Refused::NotOffered("Devin"))
        );
    }

    /// Beyond `MAX_RUNNING`, a review that's asked for waits, and starts
    /// at the PR's head as of then once one of the others ends.
    #[tokio::test]
    async fn queued_reviews_wait_for_room() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(&fx, dir.path(), STALL);
        let slug = fx.clone.slug.clone();
        let head = fx.clone.head().unwrap();
        let pr = |number| PrKey {
            repo: slug.clone(),
            number,
        };
        let busy: Vec<i64> = (10..10 + MAX_RUNNING as u64)
            .map(|n| {
                state
                    .db
                    .start_auto_review(&pr(n), "x", "t", "u", 1)
                    .unwrap()
                    .unwrap()
            })
            .collect();
        store_pr(&state, &slug, 2, &head, "me", "OPEN");
        store_pr(&state, &slug, 3, &head, "me", "OPEN");
        let id = ask(&state, &pr(2), AutoRunKind::Review)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state.db.auto_review(id).unwrap().unwrap().state,
            AutoReviewState::Queued
        );
        // A queued review is the PR's one review too.
        assert_eq!(
            ask(&state, &pr(2), AutoRunKind::Review).await.unwrap(),
            Err(Refused::Busy)
        );
        // One asked for at a commit that's no longer the head starts at
        // the head; a closed PR's is cancelled.
        let stale = state
            .db
            .queue_auto_review(&pr(3), AutoRunKind::Review, "old", "t", "u", 2)
            .unwrap()
            .unwrap();
        store_pr(&state, &slug, 4, &head, "me", "CLOSED");
        let closed = state
            .db
            .queue_auto_review(&pr(4), AutoRunKind::Review, "old", "t", "u", 3)
            .unwrap()
            .unwrap();

        start_queued(&state).await;
        assert_eq!(state.db.queued_auto_reviews().unwrap().len(), 3);

        // Two slots free up: the oldest two queued go, the closed PR's
        // review is dropped.
        for &b in &busy[..2] {
            state
                .db
                .finish_auto_review(b, AutoReviewState::Posted, None, 5)
                .unwrap();
        }
        start_queued(&state).await;
        assert!(state.db.queued_auto_reviews().unwrap().is_empty());
        for started in [id, stale] {
            let r = state.db.auto_review(started).unwrap().unwrap();
            assert_eq!(r.state, AutoReviewState::Running, "{r:?}");
            assert_eq!(r.head_oid, head);
            agent::delete(&state, &r.session.unwrap()).await.unwrap();
        }
        let r = state.db.auto_review(closed).unwrap().unwrap();
        assert_eq!(r.state, AutoReviewState::Cancelled);
        assert_eq!(
            r.error.as_deref(),
            Some("The PR was closed before it started.")
        );
    }

    /// A fixing run asked for from the PR's page takes the PR, with a
    /// lease on its head, and the PRs stacked on it; one runs per stack.
    #[tokio::test]
    async fn asking_for_a_fix() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(&fx, dir.path(), STALL);
        let slug = fx.clone.slug.clone();
        let head = fx.clone.head().unwrap();
        let pr = |number| PrKey {
            repo: slug.clone(),
            number,
        };
        let fix = AutoRunKind::Fix;

        // Nothing to address.
        store_pr(&state, &slug, 2, &head, "me", "OPEN");
        assert_eq!(
            ask(&state, &pr(2), fix).await.unwrap(),
            Err(Refused::NothingOpen)
        );
        // Someone else's PR isn't fixed either.
        store_full(
            &state,
            &slug,
            5,
            &head,
            "other",
            "OPEN",
            &[thread(1, "ann")],
            None,
        );
        assert_eq!(
            ask(&state, &pr(5), fix).await.unwrap(),
            Err(Refused::NotYours)
        );

        // A lone PR: the session takes it, so that its push has a lease.
        store_full(
            &state,
            &slug,
            2,
            &head,
            "me",
            "OPEN",
            &[thread(1, "ann")],
            None,
        );
        let id = ask(&state, &pr(2), fix).await.unwrap().unwrap();
        let run = state.db.auto_review(id).unwrap().unwrap();
        assert_eq!((run.kind, run.state), (fix, AutoReviewState::Running));
        let session = state
            .db
            .agent(&run.session.clone().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(session.kind, SessionKind::AutoFix);
        assert_eq!(session.title, "Address comments on #2: PR 2");
        let taken: Vec<_> = agent::landed_prs(&session)
            .into_iter()
            .map(|p| (p.number, p.lease))
            .collect();
        assert_eq!(taken, [(2, Some(head.clone()))]);
        assert_eq!(ask(&state, &pr(2), fix).await.unwrap(), Err(Refused::Busy));
        // A review of the same PR can still be asked for.
        let review = ask(&state, &pr(2), AutoRunKind::Review)
            .await
            .unwrap()
            .unwrap();
        for id in [id, review] {
            assert!(cancel(&state, id).await.unwrap());
        }

        // A stack: #3 on #2, #4 on #3. The comments are on #3 only, and a
        // run from #2 covers them.
        let stack = stack_of(vec![
            stack_pr(2, None, "OPEN"),
            stack_pr(3, Some(2), "OPEN"),
            stack_pr(4, Some(3), "OPEN"),
        ]);
        store_full(&state, &slug, 2, &head, "me", "OPEN", &[], Some(&stack));
        store_full(
            &state,
            &slug,
            3,
            &head,
            "me",
            "OPEN",
            &[thread(1, "ann")],
            Some(&stack),
        );
        store_full(&state, &slug, 4, &head, "me", "OPEN", &[], Some(&stack));
        assert_eq!(
            ask(&state, &pr(4), fix).await.unwrap(),
            Err(Refused::NothingOpen)
        );
        let lower = ask(&state, &pr(2), fix).await.unwrap().unwrap();
        let run = state.db.auto_review(lower).unwrap().unwrap();
        assert_eq!(run.state, AutoReviewState::Running);
        let session = state.db.agent(&run.session.unwrap()).unwrap().unwrap();
        let taken: Vec<_> = agent::landed_prs(&session)
            .iter()
            .map(|p| p.number)
            .collect();
        assert_eq!(taken, [2, 3, 4]);
        // Another one in the stack waits for it, though there's room.
        let upper = ask(&state, &pr(3), fix).await.unwrap().unwrap();
        let queued = state.db.auto_review(upper).unwrap().unwrap();
        assert_eq!(queued.state, AutoReviewState::Queued);
        assert!(cancel(&state, lower).await.unwrap());
        start_queued(&state).await;
        let run = state.db.auto_review(upper).unwrap().unwrap();
        assert_eq!(run.state, AutoReviewState::Running);
        assert!(cancel(&state, upper).await.unwrap());

        // Two PRs stacked on #2: neither chain can be picked.
        let forked = stack_of(vec![
            stack_pr(2, None, "OPEN"),
            stack_pr(3, Some(2), "OPEN"),
            stack_pr(4, Some(2), "OPEN"),
        ]);
        store_full(
            &state,
            &slug,
            2,
            &head,
            "me",
            "OPEN",
            &[thread(1, "ann")],
            Some(&forked),
        );
        assert_eq!(
            ask(&state, &pr(2), fix).await.unwrap(),
            Err(Refused::Forked { at: 2 })
        );
        // A PR above it that hasn't synced keeps it waiting.
        let unsynced = stack_of(vec![
            stack_pr(2, None, "OPEN"),
            stack_pr(9, Some(2), "OPEN"),
        ]);
        store_full(
            &state,
            &slug,
            2,
            &head,
            "me",
            "OPEN",
            &[thread(1, "ann")],
            Some(&unsynced),
        );
        assert_eq!(
            ask(&state, &pr(2), fix).await.unwrap(),
            Err(Refused::NotSynced)
        );
    }

    /// A fixing run that neither pushed nor answered is kept, and it was
    /// told how to work.
    #[tokio::test]
    async fn a_fix_that_did_nothing_is_kept() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(&fx, dir.path(), SUCCEED);
        let slug = fx.clone.slug.clone();
        let head = fx.clone.head().unwrap();
        let key = PrKey {
            repo: slug.clone(),
            number: 2,
        };
        store_full(
            &state,
            &slug,
            2,
            &head,
            "me",
            "OPEN",
            &[thread(1, "ann")],
            None,
        );
        let id = ask(&state, &key, AutoRunKind::Fix).await.unwrap().unwrap();
        let run = ended(&state).await;
        assert_eq!((run.id, run.state), (id, AutoReviewState::Unposted));
        assert!(state.db.agent(&run.session.unwrap()).unwrap().is_some());
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert!(args.contains("addresses a pull request's review"), "{args}");
        assert!(!args.contains("unattended pull request review"), "{args}");
    }

    fn loop_fix(round: u32, state: AutoReviewState, heads: &[(u64, &str)]) -> AutoReviewEntry {
        AutoReviewEntry {
            kind: AutoRunKind::Fix,
            round: Some(round),
            heads: heads.iter().map(|(n, h)| (*n, (*h).to_owned())).collect(),
            finished_at: Some(5),
            ..entry("x", state)
        }
    }

    /// The rounds count from the last push that wasn't a fixing run's.
    #[test]
    fn where_a_loop_stands() {
        let heads = |prs: &[(u64, &str)]| -> BTreeMap<u64, String> {
            prs.iter().map(|(n, h)| (*n, (*h).to_owned())).collect()
        };
        let pushed = AutoReviewState::Pushed;
        let rounds = |n| RepoSettings {
            fix_rounds: n,
            ..settings(AutoReview::NonDraft, false)
        };

        // Never ran.
        let fresh = loop_state(&[], &heads(&[(2, "a")]));
        assert_eq!(fresh, LoopState::default());
        assert!(!fresh.over(&rounds(1)) && fresh.over(&rounds(0)));
        assert_eq!(fresh.after_fix(2, "a", 3), None);

        // Its second run pushed #2 and #3 to where they are now. #4 was
        // merged since, which isn't a push.
        let fixes = [
            loop_fix(2, pushed, &[(2, "b"), (3, "c"), (4, "d")]),
            loop_fix(1, pushed, &[(2, "a")]),
        ];
        let now = heads(&[(2, "b"), (3, "c")]);
        let st = loop_state(&fixes, &now);
        assert_eq!((st.used, st.ran), (2, true));
        assert_eq!(st.after_fix(2, "b", 3), Some(true));
        assert_eq!(st.after_fix(2, "b", 2), Some(false));
        // A PR that joined the stack since isn't the loop's push.
        assert_eq!(st.after_fix(5, "e", 3), None);
        assert!(st.over(&rounds(2)) && !st.over(&rounds(3)));

        // Someone pushed to #3: the count starts over, when pushes are
        // reviewed again at all.
        let st = loop_state(&fixes, &heads(&[(2, "b"), (3, "z")]));
        assert_eq!((st.used, st.last.is_some(), st.ran), (0, false, true));
        assert_eq!(st.after_fix(2, "b", 3), None);
        assert!(st.over(&rounds(3)));
        let again = RepoSettings {
            rereview_on_push: true,
            ..rounds(3)
        };
        assert!(!st.over(&again));

        // #2 still shows the head the run found it at: GitHub is behind,
        // and that's nobody's push.
        let mut lagging = loop_fix(1, pushed, &[(2, "b"), (3, "c")]);
        lagging.start_heads = heads(&[(2, "a"), (3, "c")]);
        let st = loop_state(
            std::slice::from_ref(&lagging),
            &heads(&[(2, "a"), (3, "c")]),
        );
        assert!(st.stale && st.over(&again));
        assert_eq!(st.after_fix(2, "a", 3), None);
        let st = loop_state(
            std::slice::from_ref(&lagging),
            &heads(&[(2, "b"), (3, "c")]),
        );
        assert!(!st.stale && !st.over(&again));

        // A run that pushed nothing ends the loop until the next push.
        for ended in [
            AutoReviewState::Posted,
            AutoReviewState::Unposted,
            AutoReviewState::Failed,
        ] {
            let st = loop_state(&[loop_fix(1, ended, &[(2, "a")])], &heads(&[(2, "a")]));
            assert!(st.over(&again), "{ended:?}");
            assert_eq!(st.after_fix(2, "a", 3), None);
        }
    }

    /// A head that the loop pushed is reviewed for the next round even
    /// when pushes aren't otherwise, and not after the last round; a
    /// rebase of reviewed changes isn't reviewed again.
    #[test]
    fn due_in_a_loop() {
        let once = settings(AutoReview::NonDraft, false);
        let again = settings(AutoReview::NonDraft, true);
        let done = [entry("a", AutoReviewState::Posted)];
        let seen = |same_changes, after_fix| Seen {
            same_changes,
            after_fix,
        };
        assert!(due(
            &hit(false, "b"),
            &once,
            100,
            &done,
            0,
            seen(false, Some(true))
        ));
        assert!(!due(
            &hit(false, "b"),
            &once,
            100,
            &done,
            0,
            seen(false, Some(false))
        ));
        assert!(!due(
            &hit(false, "b"),
            &again,
            100,
            &done,
            0,
            seen(false, Some(false))
        ));
        assert!(!due(
            &hit(false, "a"),
            &once,
            100,
            &done,
            0,
            seen(false, Some(true))
        ));
        assert!(!due(
            &hit(false, "b"),
            &once,
            100,
            &done,
            0,
            seen(true, Some(true))
        ));
        assert!(!due(
            &hit(false, "b"),
            &again,
            100,
            &done,
            0,
            seen(true, None)
        ));
        // A fixing run isn't a review of the head.
        let fixed = [AutoReviewEntry {
            kind: AutoRunKind::Fix,
            ..entry("b", AutoReviewState::Pushed)
        }];
        assert!(due(
            &hit(false, "b"),
            &once,
            100,
            &fixed,
            0,
            Seen::default()
        ));
    }

    /// With rounds of review and fixes, a stack whose reviews are done
    /// gets a fixing run for what they raised, and the head it pushes
    /// gets the next round's review.
    #[tokio::test]
    async fn a_loop_goes_round() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(&fx, dir.path(), STALL);
        let slug = fx.clone.slug.clone();
        let head = fx.clone.head().unwrap();
        let key = PrKey {
            repo: slug.clone(),
            number: 2,
        };
        let at = |head: &str| Hit {
            key: key.clone(),
            head_oid: head.to_owned(),
            ..hit(false, head)
        };
        let rounds = |n, rereview| {
            let s = RepoSettings {
                fix_rounds: n,
                ..settings(AutoReview::NonDraft, rereview)
            };
            state.db.set_repo_settings(&slug, &s).unwrap();
        };
        let runs = |kind| -> Vec<AutoReviewEntry> {
            let mut all = state.db.auto_reviews_of(&key).unwrap();
            all.retain(|r| r.kind == kind);
            all
        };
        // Ends the latest run of `kind` as `state`, deleting its session.
        let end = async |kind, as_: AutoReviewState| {
            let run = runs(kind).remove(0);
            assert_eq!(run.state, AutoReviewState::Running, "{run:?}");
            let session = run.session.clone().unwrap();
            state
                .db
                .finish_auto_review(run.id, as_, None, agent::now())
                .unwrap();
            agent::delete(&state, &session).await.unwrap();
            run
        };
        rounds(2, false);

        // Another reviewer's comment doesn't get the loop going, and
        // neither does an auto-review's before the review is done.
        let mine = thread(1, "me");
        store_full(
            &state,
            &slug,
            2,
            &head,
            "me",
            "OPEN",
            &[thread(3, "ann")],
            None,
        );
        start_due(&state, &[at(&head)]).await;
        assert_eq!(runs(AutoRunKind::Review).len(), 1);
        store_full(
            &state,
            &slug,
            2,
            &head,
            "me",
            "OPEN",
            std::slice::from_ref(&mine),
            None,
        );
        start_due(&state, &[at(&head)]).await;
        assert!(runs(AutoRunKind::Fix).is_empty());
        end(AutoRunKind::Review, AutoReviewState::Posted).await;
        store_full(
            &state,
            &slug,
            2,
            &head,
            "me",
            "OPEN",
            &[thread(3, "ann")],
            None,
        );
        start_due(&state, &[at(&head)]).await;
        assert!(runs(AutoRunKind::Fix).is_empty());

        // Round 1's fixing run addresses the review's comment.
        store_full(
            &state,
            &slug,
            2,
            &head,
            "me",
            "OPEN",
            std::slice::from_ref(&mine),
            None,
        );
        start_due(&state, &[at(&head)]).await;
        let fix = runs(AutoRunKind::Fix).remove(0);
        assert_eq!((fix.round, fix.manual), (Some(1), false));
        assert_eq!(fix.heads, BTreeMap::from([(2, head.clone())]));
        assert_eq!(fix.start_heads, fix.heads);
        // While it runs, nothing else starts.
        start_due(&state, &[at(&head)]).await;
        assert_eq!(
            (
                runs(AutoRunKind::Fix).len(),
                runs(AutoRunKind::Review).len()
            ),
            (1, 1)
        );

        // It pushed: the new head is reviewed, though pushes otherwise
        // aren't, and round 2's fixing run follows that review.
        let second = crate::git::tests::commit(&fx.clone.path, "second", "second\n");
        record_heads(
            &state,
            fix.session.as_deref().unwrap(),
            &BTreeMap::from([(2, second.clone())]),
            false,
        )
        .unwrap();
        end(AutoRunKind::Fix, AutoReviewState::Pushed).await;
        store_full(
            &state,
            &slug,
            2,
            &second,
            "me",
            "OPEN",
            std::slice::from_ref(&mine),
            None,
        );
        start_due(&state, &[at(&second)]).await;
        assert!(runs(AutoRunKind::Fix).len() == 1, "the review comes first");
        let review = end(AutoRunKind::Review, AutoReviewState::Posted).await;
        assert_eq!(review.head_oid, second);
        start_due(&state, &[at(&second)]).await;
        let fix = runs(AutoRunKind::Fix).remove(0);
        assert_eq!(fix.round, Some(2));

        // After the last round's push nothing more happens.
        let third = crate::git::tests::commit(&fx.clone.path, "third", "third\n");
        record_heads(
            &state,
            fix.session.as_deref().unwrap(),
            &BTreeMap::from([(2, third.clone())]),
            false,
        )
        .unwrap();
        end(AutoRunKind::Fix, AutoReviewState::Pushed).await;
        store_full(
            &state,
            &slug,
            2,
            &third,
            "me",
            "OPEN",
            std::slice::from_ref(&mine),
            None,
        );
        start_due(&state, &[at(&third)]).await;
        assert_eq!(
            (
                runs(AutoRunKind::Fix).len(),
                runs(AutoRunKind::Review).len()
            ),
            (2, 2)
        );

        // Nor after the user's own push, unless pushes are reviewed
        // again: then the loop starts over.
        let fourth = crate::git::tests::commit(&fx.clone.path, "fourth", "fourth\n");
        store_full(
            &state,
            &slug,
            2,
            &fourth,
            "me",
            "OPEN",
            std::slice::from_ref(&mine),
            None,
        );
        start_due(&state, &[at(&fourth)]).await;
        assert_eq!(
            (
                runs(AutoRunKind::Fix).len(),
                runs(AutoRunKind::Review).len()
            ),
            (2, 2)
        );
        rounds(2, true);
        start_due(&state, &[at(&fourth)]).await;
        assert_eq!(runs(AutoRunKind::Review).len(), 3);
        end(AutoRunKind::Review, AutoReviewState::Posted).await;
        start_due(&state, &[at(&fourth)]).await;
        let fix = runs(AutoRunKind::Fix).remove(0);
        assert_eq!(fix.round, Some(1));
        end(AutoRunKind::Fix, AutoReviewState::Cancelled).await;
    }

    /// A queued review can be cancelled before it starts.
    #[tokio::test]
    async fn cancelling_a_queued_review() {
        let fx = crate::git::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::agent::tests::state_with_claude(&fx, dir.path(), STALL);
        let key = PrKey {
            repo: fx.clone.slug.clone(),
            number: 2,
        };
        let id = state
            .db
            .queue_auto_review(&key, AutoRunKind::Review, "a", "t", "u", 1)
            .unwrap()
            .unwrap();
        assert!(cancel(&state, id).await.unwrap());
        let r = state.db.auto_review(id).unwrap().unwrap();
        assert_eq!(r.state, AutoReviewState::Cancelled);
        assert!(r.finished_at.is_some());
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
        let mut info = review_info("me");
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
