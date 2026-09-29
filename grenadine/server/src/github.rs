//! The GitHub API: searches, PR details, the head branch's activity log and
//! review comments.

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use chrono::DateTime;
use grenadine_core::api::{PrKey, ReviewComment, Side};
use grenadine_core::versions::{Activity, ActivityKind, ForcePush};
use reqwest::header;
use serde::Deserialize;
use serde_json::{Value, json};

const API: &str = "https://api.github.com";

/// Reads the token of the `gh` CLI's login.
pub fn gh_token() -> Result<String> {
    let out = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .output()
        .context("failed to run `gh auth token`; is the gh CLI installed?")?;
    if !out.status.success() {
        bail!(
            "`gh auth token` failed ({}); run `gh auth login`",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
}

/// One PR that a search found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub key: PrKey,
    pub updated_at: String,
    pub head_oid: String,
}

/// Everything about a PR that its sync needs from GraphQL.
#[derive(Clone, Debug)]
pub struct PrData {
    pub title: String,
    pub body: String,
    pub author: String,
    pub state: String,
    pub is_draft: bool,
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
    pub base_ref: String,
    pub head_ref: String,
    pub head_oid: String,
    /// `owner/name` of the repository the head branch lives in; `None` when
    /// it was deleted.
    pub head_repo: Option<String>,
    pub force_pushes: Vec<ForcePush>,
    /// The PR's commits in order, each with the time its first check suite
    /// was created (a stand-in for when it was pushed).
    pub commits: Vec<(String, Option<i64>)>,
}

/// Parses an RFC 3339 timestamp into Unix seconds.
pub fn unix(ts: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(ts)
        .with_context(|| format!("bad timestamp {ts}"))?
        .timestamp())
}

/// Builds the GitHub search query for an inbox filter: `is:pr` plus one
/// `repo:` qualifier per configured repository.
pub fn search_query(filter: &str, repos: &[String]) -> String {
    let mut q = format!("is:pr {}", filter.trim());
    for r in repos {
        q += &format!(" repo:{r}");
    }
    q
}

const SEARCH_LIMIT: usize = 100;

const PR_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      title body url state isDraft createdAt updatedAt
      author { login }
      baseRefName headRefName headRefOid
      headRepository { nameWithOwner }
      timelineItems(first: 100, itemTypes: [HEAD_REF_FORCE_PUSHED_EVENT]) {
        nodes {
          ... on HeadRefForcePushedEvent {
            createdAt
            beforeCommit { oid }
            afterCommit { oid }
          }
        }
      }
      commits(first: 100) {
        nodes { commit { oid checkSuites(first: 10) { nodes { createdAt } } } }
      }
    }
  }
}
"#;

#[derive(Deserialize)]
struct RestComment {
    id: u64,
    in_reply_to_id: Option<u64>,
    user: Option<Login>,
    body: String,
    path: String,
    original_commit_id: String,
    original_line: Option<u32>,
    original_start_line: Option<u32>,
    line: Option<u32>,
    start_line: Option<u32>,
    side: Option<String>,
    subject_type: Option<String>,
    created_at: String,
    html_url: String,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct RestActivity {
    before: String,
    after: String,
    timestamp: String,
    activity_type: String,
}

fn str_at<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or_default()
}

/// The URL of the `rel="next"` page in a `Link` header.
fn next_link(headers: &header::HeaderMap) -> Option<String> {
    let link = headers.get(header::LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_owned()
        })
    })
}

impl GitHub {
    pub fn new(token: &str) -> Result<GitHub> {
        let mut headers = header::HeaderMap::new();
        let mut auth = header::HeaderValue::from_str(&format!("Bearer {token}"))?;
        auth.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth);
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            header::HeaderValue::from_static("2022-11-28"),
        );
        let http = reqwest::Client::builder()
            .user_agent("grenadine")
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(GitHub { http })
    }

    /// Runs a GraphQL query. Returns `data` and the `errors`, if any.
    async fn graphql(&self, query: &str, variables: Value) -> Result<(Value, Vec<Value>)> {
        let resp: Value = self
            .http
            .post(format!("{API}/graphql"))
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let errors = resp
            .get("errors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok((resp.get("data").cloned().unwrap_or(Value::Null), errors))
    }

    /// Runs several searches in one GraphQL request. Each result is either
    /// the PRs found, in GitHub's order, or the error for that search.
    pub async fn search(
        &self,
        queries: &[String],
    ) -> Result<Vec<std::result::Result<Vec<Hit>, String>>> {
        if queries.is_empty() {
            return Ok(Vec::new());
        }
        let mut gql = String::from("query(");
        gql += &(0..queries.len())
            .map(|i| format!("$q{i}: String!"))
            .collect::<Vec<_>>()
            .join(", ");
        gql += ") {\n";
        let mut vars = serde_json::Map::new();
        for (i, q) in queries.iter().enumerate() {
            gql += &format!(
                "s{i}: search(type: ISSUE, query: $q{i}, first: {SEARCH_LIMIT}) {{
                   nodes {{ ... on PullRequest {{ number updatedAt headRefOid repository {{ nameWithOwner }} }} }}
                 }}\n"
            );
            vars.insert(format!("q{i}"), json!(q));
        }
        gql += "}";
        let (data, errors) = self.graphql(&gql, Value::Object(vars)).await?;
        let mut error_for: BTreeMap<String, String> = BTreeMap::new();
        for e in &errors {
            let alias = e
                .pointer("/path/0")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let msg = e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            error_for
                .entry(alias.to_owned())
                .or_insert_with(|| msg.to_owned());
        }
        Ok((0..queries.len())
            .map(|i| {
                let alias = format!("s{i}");
                let Some(nodes) = data
                    .pointer(&format!("/{alias}/nodes"))
                    .and_then(Value::as_array)
                else {
                    return Err(error_for
                        .get(&alias)
                        .or_else(|| error_for.get(""))
                        .cloned()
                        .unwrap_or_else(|| "search failed".to_owned()));
                };
                Ok(nodes
                    .iter()
                    .filter(|n| n.get("number").is_some())
                    .map(|n| Hit {
                        key: PrKey {
                            repo: str_at(n, "/repository/nameWithOwner").to_owned(),
                            number: n["number"].as_u64().unwrap_or_default(),
                        },
                        updated_at: str_at(n, "/updatedAt").to_owned(),
                        head_oid: str_at(n, "/headRefOid").to_owned(),
                    })
                    .collect())
            })
            .collect())
    }

    pub async fn pr(&self, key: &PrKey) -> Result<PrData> {
        let (owner, name) = key
            .repo
            .split_once('/')
            .ok_or_else(|| anyhow!("bad repo {}", key.repo))?;
        let (data, errors) = self
            .graphql(
                PR_QUERY,
                json!({ "owner": owner, "name": name, "number": key.number }),
            )
            .await?;
        let pr = data
            .pointer("/repository/pullRequest")
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                anyhow!(
                    "can't read {}#{}: {}",
                    key.repo,
                    key.number,
                    errors
                        .first()
                        .and_then(|e| e["message"].as_str())
                        .unwrap_or("not found")
                )
            })?;

        let mut force_pushes = Vec::new();
        for n in pr
            .pointer("/timelineItems/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let after = str_at(n, "/afterCommit/oid");
            if after.is_empty() {
                continue;
            }
            force_pushes.push(ForcePush {
                timestamp: unix(str_at(n, "/createdAt"))?,
                before: Some(str_at(n, "/beforeCommit/oid"))
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                after: after.to_owned(),
            });
        }
        let commits = pr
            .pointer("/commits/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|n| {
                let first_suite = n
                    .pointer("/commit/checkSuites/nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|s| unix(str_at(s, "/createdAt")).ok())
                    .min();
                (str_at(n, "/commit/oid").to_owned(), first_suite)
            })
            .collect();

        Ok(PrData {
            title: str_at(pr, "/title").to_owned(),
            body: str_at(pr, "/body").to_owned(),
            author: Some(str_at(pr, "/author/login"))
                .filter(|s| !s.is_empty())
                .unwrap_or("ghost")
                .to_owned(),
            state: str_at(pr, "/state").to_owned(),
            is_draft: pr["isDraft"].as_bool().unwrap_or(false),
            url: str_at(pr, "/url").to_owned(),
            created_at: str_at(pr, "/createdAt").to_owned(),
            updated_at: str_at(pr, "/updatedAt").to_owned(),
            base_ref: str_at(pr, "/baseRefName").to_owned(),
            head_ref: str_at(pr, "/headRefName").to_owned(),
            head_oid: str_at(pr, "/headRefOid").to_owned(),
            head_repo: Some(str_at(pr, "/headRepository/nameWithOwner"))
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            force_pushes,
            commits,
        })
    }

    /// GETs every page of a REST list.
    async fn get_all<T: serde::de::DeserializeOwned>(&self, url: String) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut next = Some(url);
        while let Some(url) = next.take() {
            let resp = self.http.get(&url).send().await?.error_for_status()?;
            next = next_link(resp.headers());
            out.extend(resp.json::<Vec<T>>().await?);
        }
        Ok(out)
    }

    /// The activity log of a branch, oldest first. Empty when GitHub has no
    /// log for it, e.g. because the repository is gone.
    pub async fn activity(&self, repo: &str, branch: &str) -> Result<Vec<Activity>> {
        let url = format!(
            "{API}/repos/{repo}/activity?ref=refs/heads/{branch}&direction=asc&per_page=100"
        );
        let raw: Vec<RestActivity> = match self.get_all(url).await {
            Ok(raw) => raw,
            Err(e)
                if e.downcast_ref::<reqwest::Error>()
                    .and_then(reqwest::Error::status)
                    == Some(reqwest::StatusCode::NOT_FOUND) =>
            {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let mut out = raw
            .into_iter()
            .map(|a| {
                Ok(Activity {
                    timestamp: unix(&a.timestamp)?,
                    kind: match a.activity_type.as_str() {
                        "push" => ActivityKind::Push,
                        "force_push" => ActivityKind::ForcePush,
                        "branch_creation" => ActivityKind::BranchCreation,
                        "branch_deletion" => ActivityKind::BranchDeletion,
                        _ => ActivityKind::Other,
                    },
                    before: a.before,
                    after: a.after,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // `direction=asc` should already give this order; don't rely on it.
        out.sort_by_key(|a| a.timestamp);
        Ok(out)
    }

    pub async fn review_comments(&self, key: &PrKey) -> Result<Vec<ReviewComment>> {
        let url = format!(
            "{API}/repos/{}/pulls/{}/comments?per_page=100",
            key.repo, key.number
        );
        let raw: Vec<RestComment> = self.get_all(url).await?;
        Ok(raw
            .into_iter()
            .map(|c| ReviewComment {
                id: c.id,
                in_reply_to: c.in_reply_to_id,
                author: c
                    .user
                    .map(|u| u.login)
                    .unwrap_or_else(|| "ghost".to_owned()),
                body: c.body,
                path: c.path,
                original_commit: c.original_commit_id,
                original_line: c.original_line,
                original_start_line: c.original_start_line,
                line: c.line,
                start_line: c.start_line,
                side: if c.side.as_deref() == Some("LEFT") {
                    Side::Left
                } else {
                    Side::Right
                },
                on_file: c.subject_type.as_deref() == Some("file"),
                created_at: c.created_at,
                url: c.html_url,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_scoped_to_the_repos() {
        assert_eq!(
            search_query(" is:open author:@me ", &["a/b".into(), "c/d".into()]),
            "is:pr is:open author:@me repo:a/b repo:c/d"
        );
    }

    #[test]
    fn parses_link_headers() {
        let mut h = header::HeaderMap::new();
        h.insert(
            header::LINK,
            header::HeaderValue::from_static(
                r#"<https://x/?page=2>; rel="next", <https://x/?page=5>; rel="last""#,
            ),
        );
        assert_eq!(next_link(&h).as_deref(), Some("https://x/?page=2"));
        h.insert(
            header::LINK,
            header::HeaderValue::from_static(r#"<https://x/?page=1>; rel="prev""#),
        );
        assert_eq!(next_link(&h), None);
    }

    #[test]
    fn timestamps() {
        assert_eq!(unix("1970-01-01T00:01:00Z").unwrap(), 60);
        assert!(unix("nope").is_err());
    }
}
