//! The pipeline an inbox's filter goes through, shared between the
//! poller and validate_inboxes: prepare parses the filter and widens it
//! into a GitHub query, run fetches the pages in batched rounds, and the
//! result keeps the hits that match locally, title-sorted if asked.

use std::collections::BTreeSet;

use anyhow::Result;
use grenadine_core::api::Truncation;
use grenadine_core::filter::{self, Filter, TitleSort};

use crate::github::{self, GitHub, Hit};

/// The most hits GitHub returns for one search. Filters with `gr:`
/// terms fetch up to it because the widened query usually finds more
/// PRs than end up in the inbox; plain filters fetch one page.
pub const LOCAL_LIMIT: usize = 1000;

/// How many searches one GraphQL request carries. With the review
/// fields the local filters need, a bigger request exceeds GitHub's
/// resource limit and gets a 502 for the whole batch.
const SEARCH_BATCH: usize = 5;

type SearchResults = Vec<std::result::Result<github::SearchPage, String>>;
type SearchFut<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(String, SearchResults)>> + Send + 'a>,
>;

/// Runs `requests` in batches of SEARCH_BATCH, in order; the viewer's
/// login comes from the first batch that answers. A request that fails
/// at the HTTP level is retried as two halves, recursively — one heavy
/// search can sink its whole request, like Pearl's timeout bisection,
/// while its siblings may be fine. A lone search gets one retry before
/// it becomes its inbox's error.
pub async fn search_batched(
    github: &GitHub,
    requests: &[(String, Option<String>, bool)],
) -> Result<(String, SearchResults)> {
    search_batched_with(requests, &|reqs| Box::pin(github.search(reqs))).await
}

async fn search_batched_with<'a, F>(
    requests: &'a [(String, Option<String>, bool)],
    search: &'a F,
) -> Result<(String, SearchResults)>
where
    F: Fn(&'a [(String, Option<String>, bool)]) -> SearchFut<'a>,
{
    let mut viewer = String::new();
    // Results land by request index so bisected retries can't reorder
    // the inboxes.
    let mut out: Vec<Option<std::result::Result<github::SearchPage, String>>> =
        (0..requests.len()).map(|_| None).collect();
    // (start index, searches) pairs; halves go back on the queue.
    type SearchList<'a> = &'a [(String, Option<String>, bool)];
    let mut pending: Vec<(usize, SearchList<'_>)> = requests
        .chunks(SEARCH_BATCH)
        .enumerate()
        .map(|(i, c)| (i * SEARCH_BATCH, c))
        .collect();
    fn take(
        viewer: &mut String,
        out: &mut [Option<std::result::Result<github::SearchPage, String>>],
        start: usize,
        v: String,
        results: SearchResults,
    ) {
        if viewer.is_empty() {
            *viewer = v;
        }
        for (j, r) in results.into_iter().enumerate() {
            out[start + j] = Some(r);
        }
    }
    while let Some((start, chunk)) = pending.pop() {
        match search(chunk).await {
            Ok((v, results)) => take(&mut viewer, &mut out, start, v, results),
            Err(_) if chunk.len() > 1 => {
                let mid = chunk.len() / 2;
                pending.push((start + mid, &chunk[mid..]));
                pending.push((start, &chunk[..mid]));
            }
            Err(_) => match search(chunk).await {
                Ok((v, results)) => take(&mut viewer, &mut out, start, v, results),
                Err(e) => out[start] = Some(Err(format!("{e:#}"))),
            },
        }
    }
    Ok((
        viewer,
        out.into_iter()
            .map(|r| r.unwrap_or_else(|| Err("search did not run".into())))
            .collect(),
    ))
}

/// A filter compiled for one inbox: the query to send GitHub and, when
/// the filter had `gr:` terms, the parsed expression for the local
/// evaluation.
pub struct Prepared {
    pub query: String,
    pub local: Option<Filter>,
}

/// Parses `filter` and builds the GitHub query for it. A parse error is
/// the String the inbox shows.
pub fn prepare(filter: &str, slugs: &[String]) -> std::result::Result<Prepared, String> {
    if !filter::has_local_terms(filter) {
        return Ok(Prepared {
            query: github::search_query(filter, slugs),
            local: None,
        });
    }
    let parsed = filter::parse(filter)?;
    Ok(Prepared {
        query: github::search_query(&parsed.search_terms(), slugs),
        local: Some(parsed),
    })
}

/// One inbox's finished search: the hits to list and, when GitHub found
/// more than were fetched, how much of the result that is.
pub struct InboxResult {
    pub hits: Vec<Hit>,
    pub truncated: Option<Truncation>,
}

/// Runs every inbox's search. The first round fetches each search's
/// first page in one request; each further round fetches the next page
/// of every local inbox that has one, until all are done or hit
/// LOCAL_LIMIT. Returns the viewer's login and, per prepared inbox, its
/// finished result or its search's error.
pub async fn run(
    github: &GitHub,
    prepared: &[Prepared],
    repos: &BTreeSet<String>,
) -> Result<(String, Vec<std::result::Result<InboxResult, String>>)> {
    struct Acc {
        hits: Vec<Hit>,
        issue_count: u64,
        after: Option<String>,
        more: bool,
    }
    let requests: Vec<(String, Option<String>, bool)> = prepared
        .iter()
        .map(|p| (p.query.clone(), None, p.local.is_some()))
        .collect();
    let (viewer, results) = search_batched(github, &requests).await?;
    let mut accs: Vec<std::result::Result<Acc, String>> = results
        .into_iter()
        .map(|r| {
            r.map(|p| Acc {
                hits: p.hits,
                issue_count: p.issue_count,
                after: p.end_cursor,
                more: p.has_next_page,
            })
        })
        .collect();
    loop {
        let pending: Vec<usize> = (0..prepared.len())
            .filter(|&i| {
                prepared[i].local.is_some()
                    && accs[i]
                        .as_ref()
                        .is_ok_and(|a| a.more && a.hits.len() < LOCAL_LIMIT)
            })
            .collect();
        if pending.is_empty() {
            break;
        }
        let requests: Vec<(String, Option<String>, bool)> = pending
            .iter()
            .map(|&i| {
                (
                    prepared[i].query.clone(),
                    accs[i].as_ref().unwrap().after.clone(),
                    true,
                )
            })
            .collect();
        let (_, results) = search_batched(github, &requests).await?;
        for (i, r) in pending.into_iter().zip(results) {
            match r {
                Ok(p) => {
                    let acc = accs[i].as_mut().unwrap();
                    acc.hits.extend(p.hits);
                    acc.issue_count = p.issue_count;
                    acc.after = p.end_cursor;
                    acc.more = p.has_next_page;
                }
                Err(e) => accs[i] = Err(e),
            }
        }
    }

    let out = prepared
        .iter()
        .zip(accs)
        .map(|(p, acc)| acc.map(|a| finish(p, a.hits, a.issue_count, viewer.as_str(), repos)))
        .collect();
    Ok((viewer, out))
}

/// What one inbox's fetched hits become: only the configured
/// repositories, then, for a local filter, the hits it matches in its
/// title sort. `issue_count` is how many the search found; more than
/// were fetched truncates the inbox.
fn finish(
    p: &Prepared,
    hits: Vec<Hit>,
    issue_count: u64,
    viewer: &str,
    repos: &BTreeSet<String>,
) -> InboxResult {
    // How many hits were fetched before local filtering decides the
    // truncation, not how many made it.
    let fetched = hits.len();
    let mut hits: Vec<Hit> = hits
        .into_iter()
        .filter(|h| repos.contains(&h.key.repo))
        .collect();
    if let Some(local) = &p.local {
        hits.retain(|h| local.matches(&h.facts, viewer));
        match local.title_sort() {
            Some(TitleSort::Asc) => hits.sort_by(|a, b| {
                a.title
                    .to_lowercase()
                    .cmp(&b.title.to_lowercase())
                    .then_with(|| a.title.cmp(&b.title))
            }),
            Some(TitleSort::Desc) => hits.sort_by(|a, b| {
                b.title
                    .to_lowercase()
                    .cmp(&a.title.to_lowercase())
                    .then_with(|| b.title.cmp(&a.title))
            }),
            None => {}
        }
    }
    InboxResult {
        hits,
        truncated: (issue_count > fetched as u64).then_some(Truncation {
            fetched: fetched as u32,
            found: issue_count as u32,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use grenadine_core::api::PrKey;
    use grenadine_core::filter::PrFacts;

    use super::*;

    fn hit(repo: &str, number: u64, title: &str) -> Hit {
        Hit {
            key: PrKey {
                repo: repo.into(),
                number,
            },
            title: title.into(),
            author: "a".into(),
            state: "OPEN".into(),
            is_draft: false,
            url: "u".into(),
            updated_at: "u".into(),
            head_oid: "h".into(),
            threads: Default::default(),
            ci: None,
            review_decision: None,
            facts: PrFacts {
                title: title.into(),
                author: "a".into(),
                state: "OPEN".into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn finish_truncates_by_fetched_not_matched() {
        let repos: BTreeSet<String> = ["o/n".to_owned()].into_iter().collect();
        let p = Prepared {
            query: String::new(),
            local: None,
        };
        let hits = vec![hit("o/n", 1, "x"), hit("o/m", 2, "y")];
        let r = finish(&p, hits, 250, "me", &repos);
        assert_eq!(r.hits.len(), 1);
        assert_eq!(
            r.truncated,
            Some(Truncation {
                fetched: 2,
                found: 250
            })
        );
        // At or under the fetched count there's no truncation.
        let r = finish(&p, vec![hit("o/n", 1, "x")], 1, "me", &repos);
        assert_eq!(r.truncated, None);
    }

    #[test]
    fn finish_matches_and_title_sorts() {
        let repos: BTreeSet<String> = ["o/n".to_owned()].into_iter().collect();
        let p = prepare("gr:review:active gr:sort:title-asc", &[]).unwrap();
        let mut a = hit("o/n", 1, "Zebra");
        a.facts.reviews.push(grenadine_core::filter::ReviewFact {
            author: "bob".into(),
            state: "COMMENTED".into(),
        });
        let b = hit("o/n", 2, "apple");
        let mut c2 = hit("o/n", 3, "Mango");
        c2.facts.reviews.push(grenadine_core::filter::ReviewFact {
            author: "bob".into(),
            state: "APPROVED".into(),
        });
        // b has no review, so it's out; a and c2 sort by title.
        let r = finish(&p, vec![a, b, c2], 3, "me", &repos);
        let titles: Vec<&str> = r.hits.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, ["Mango", "Zebra"]);
    }

    fn page() -> github::SearchPage {
        github::SearchPage {
            hits: Vec::new(),
            issue_count: 0,
            has_next_page: false,
            end_cursor: None,
        }
    }

    /// A stub search: requests with more than two searches fail (like
    /// an overloaded request), the "bad" query always fails, the
    /// "flaky" query fails once. Records every call's size.
    #[allow(clippy::type_complexity)]
    fn stub() -> (
        impl for<'a> Fn(&'a [(String, Option<String>, bool)]) -> SearchFut<'a> + Send + Sync,
        Arc<Mutex<Vec<usize>>>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls2 = calls.clone();
        let flaky_seen = Arc::new(Mutex::new(false));
        (
            move |reqs| {
                let calls = calls.clone();
                let flaky_seen = flaky_seen.clone();
                let reqs: Vec<(String, Option<String>, bool)> = reqs.to_vec();
                Box::pin(async move {
                    calls.lock().unwrap().push(reqs.len());
                    let flaky_now = reqs.iter().any(|(q, _, _)| q == "flaky")
                        && !std::mem::replace(&mut *flaky_seen.lock().unwrap(), true);
                    if reqs.len() > 2 || reqs.iter().any(|(q, _, _)| q == "bad") || flaky_now {
                        anyhow::bail!("502")
                    }
                    Ok(("me".to_owned(), reqs.iter().map(|_| Ok(page())).collect()))
                })
            },
            calls2,
        )
    }

    #[tokio::test]
    async fn search_bisects_failed_requests() {
        let requests: Vec<(String, Option<String>, bool)> = (0..9)
            .map(|i| (format!("q{i}"), None, false))
            .chain(std::iter::once(("flaky".to_owned(), None, false)))
            .chain(std::iter::once(("bad".to_owned(), None, false)))
            .collect();
        let (search, calls) = stub();
        let (viewer, results) = search_batched_with(&requests, &search).await.unwrap();
        assert_eq!(viewer, "me");
        // Only "bad" errored; every other search got its page.
        assert!(results[..10].iter().all(|r| r.is_ok()));
        assert!(results[10].is_err());
        let calls = calls.lock().unwrap();
        // The 5-chunk holding "bad" failed, halved to 2+3, the 3
        // halved to 1+2, and "bad" ran once more alone.
        assert!(calls.contains(&5));
        assert!(calls.contains(&3));
        assert!(calls.contains(&1));
    }
}
