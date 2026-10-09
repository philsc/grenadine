//! Checks the default inboxes' filters against live GitHub search
//! results.

#[allow(dead_code)]
#[path = "../github.rs"]
mod github;
#[path = "../inboxes.rs"]
mod inboxes;

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use github::Hit;
use grenadine_core::api::PrKey;

#[derive(Parser)]
#[command(about = "Validate the default inboxes against live GitHub search")]
struct Args {
    /// A GitHub repository as OWNER/NAME. Repeat to cover several.
    #[arg(long = "repo", required = true, value_name = "OWNER/NAME")]
    repos: Vec<String>,
}

const MINE: &str = "state:open archived:false author:@me";
const MINE_QUEUED: &str = "state:open archived:false author:@me is:queued";
const MINE_CR: &str = "state:open archived:false draft:false author:@me review:changes_requested";
const MINE_APPROVED: &str = "state:open archived:false draft:false author:@me review:approved";
const MINE_NONE: &str = "state:open archived:false draft:false author:@me review:none";
const USER_REQ: &str = "state:open archived:false user-review-requested:@me";
const TEAM_REQ: &str = "state:open archived:false team-review-requested-user:@me";
const REVIEWED: &str = "state:open archived:false reviewed-by:@me -author:@me";
const REVIEWED_QUEUED: &str = "state:open archived:false reviewed-by:@me -author:@me is:queued";

/// The default inboxes, in DEFAULT_INBOXES order.
const INBOX_SHORT: &[&str] = &["NR", "NTR", "RET", "APP", "WR", "DR", "MRG", "DNR", "WA"];

fn keys(hits: &[Hit]) -> BTreeSet<PrKey> {
    hits.iter().map(|h| h.key.clone()).collect()
}

fn nondraft(hits: &[Hit]) -> BTreeSet<PrKey> {
    hits.iter()
        .filter(|h| !h.is_draft)
        .map(|h| h.key.clone())
        .collect()
}

fn draft(hits: &[Hit]) -> BTreeSet<PrKey> {
    hits.iter()
        .filter(|h| h.is_draft)
        .map(|h| h.key.clone())
        .collect()
}

fn describe(key: &PrKey, by_key: &BTreeMap<PrKey, Hit>) -> String {
    match by_key.get(key) {
        Some(h) => format!(
            "{}#{}{} {}",
            h.key.repo,
            h.key.number,
            if h.is_draft { " [draft]" } else { "" },
            h.title
        ),
        None => format!("{}#{}", key.repo, key.number),
    }
}

/// The lines a FAIL prints: which PRs each side of the comparison has
/// that the other lacks.
fn diff_lines(
    a_label: &str,
    a: &BTreeSet<PrKey>,
    b_label: &str,
    b: &BTreeSet<PrKey>,
    by_key: &BTreeMap<PrKey, Hit>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (label, keys) in [
        (format!("only in {a_label}"), a - b),
        (format!("only in {b_label}"), b - a),
    ] {
        if !keys.is_empty() {
            lines.push(format!("{label}:"));
            lines.extend(keys.iter().map(|k| format!("  {}", describe(k, by_key))));
        }
    }
    lines
}

struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn check(&mut self, ok: bool, desc: &str, lines: Vec<String>) {
        if ok {
            self.passed += 1;
            println!("PASS {desc}");
        } else {
            self.failed += 1;
            println!("FAIL {desc}");
            for line in lines {
                println!("  {line}");
            }
        }
    }

    /// `want` is the set computed from reference queries, `got` the
    /// inbox's.
    fn check_eq(
        &mut self,
        desc: &str,
        want: &BTreeSet<PrKey>,
        got: &BTreeSet<PrKey>,
        by_key: &BTreeMap<PrKey, Hit>,
    ) {
        self.check(
            want == got,
            desc,
            diff_lines("expected", want, desc, got, by_key),
        );
    }
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();
    let token = github::gh_token()?;
    let gh = github::GitHub::new(&token, github::API)?;

    let mut queries: Vec<(String, String)> = Vec::new();
    for (label, filter) in [
        ("MINE", MINE),
        ("MINE_QUEUED", MINE_QUEUED),
        ("MINE_CR", MINE_CR),
        ("MINE_APPROVED", MINE_APPROVED),
        ("MINE_NONE", MINE_NONE),
        ("USER_REQ", USER_REQ),
        ("TEAM_REQ", TEAM_REQ),
        ("REVIEWED", REVIEWED),
        ("REVIEWED_QUEUED", REVIEWED_QUEUED),
    ] {
        queries.push((label.to_owned(), github::search_query(filter, &args.repos)));
    }
    for (short, (_, filter)) in INBOX_SHORT.iter().zip(inboxes::DEFAULT_INBOXES.iter()) {
        queries.push((short.to_string(), github::search_query(filter, &args.repos)));
    }
    for repo in &args.repos {
        queries.push((
            format!("MINE_{repo}"),
            github::search_query(MINE, std::slice::from_ref(repo)),
        ));
    }

    let results = gh
        .search(&queries.iter().map(|(_, q)| q.clone()).collect::<Vec<_>>())
        .await?;

    let mut report = Report {
        passed: 0,
        failed: 0,
    };
    // Every PR seen, for printing; a result that failed leaves its
    // label out of `hits` so dependent checks can skip.
    let mut hits: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
    let mut by_key: BTreeMap<PrKey, Hit> = BTreeMap::new();
    for ((label, query), result) in queries.iter().zip(results) {
        match result {
            Ok(found) => {
                println!("INFO {label}: {} hits", found.len());
                if found.len() == github::SEARCH_LIMIT {
                    println!(
                        "WARN {query} hit the {}-result limit; set checks involving it are inconclusive",
                        github::SEARCH_LIMIT
                    );
                }
                for h in &found {
                    by_key.entry(h.key.clone()).or_insert_with(|| h.clone());
                }
                hits.insert(label.clone(), found);
            }
            Err(e) => report.check(false, &format!("{label}: {query}"), vec![e]),
        }
    }

    let have = |labels: &[&str]| labels.iter().all(|l| hits.contains_key(*l));
    let skip = |desc: &str, labels: &[&str]| -> bool {
        if have(labels) {
            return false;
        }
        let missing: Vec<_> = labels.iter().filter(|l| !hits.contains_key(**l)).collect();
        println!("SKIP {desc} ({missing:?} unavailable)");
        true
    };
    let set = |label: &str| keys(&hits[label]);

    // The inboxes DR, RET, APP, WR and MRG must partition MINE.
    let mine_members = ["DR", "RET", "APP", "WR", "MRG"];
    if have(&mine_members) {
        for (i, a) in mine_members.iter().enumerate() {
            for b in &mine_members[i + 1..] {
                let overlap = &set(a) & &set(b);
                report.check(
                    overlap.is_empty(),
                    &format!("{a} and {b} are disjoint"),
                    overlap.iter().map(|k| describe(k, &by_key)).collect(),
                );
            }
        }
        let union: BTreeSet<PrKey> = mine_members.iter().flat_map(|l| set(l)).collect();
        if have(&["MINE"]) {
            report.check_eq(
                "DR ∪ RET ∪ APP ∪ WR ∪ MRG == MINE",
                &union,
                &set("MINE"),
                &by_key,
            );
        } else {
            println!("SKIP DR ∪ RET ∪ APP ∪ WR ∪ MRG == MINE (MINE unavailable)");
        }
    } else {
        println!("SKIP partition checks (some of {mine_members:?} unavailable)");
    }

    if !skip(
        "RET == authored, reviewed, not queued, not approved",
        &["RET", "MINE", "MINE_QUEUED", "MINE_APPROVED", "MINE_NONE"],
    ) {
        let mut want = nondraft(&hits["MINE"]);
        for l in ["MINE_QUEUED", "MINE_APPROVED", "MINE_NONE"] {
            want = &want - &set(l);
        }
        report.check_eq(
            "RET == (nondraft(MINE) \\ MINE_QUEUED) \\ MINE_APPROVED \\ MINE_NONE",
            &want,
            &set("RET"),
            &by_key,
        );
    }
    if !skip(
        "changes-requested PRs are in RET",
        &["MINE_CR", "MINE_QUEUED", "RET"],
    ) {
        let missing = &(&set("MINE_CR") - &set("MINE_QUEUED")) - &set("RET");
        report.check(
            missing.is_empty(),
            "MINE_CR \\ MINE_QUEUED ⊆ RET",
            missing.iter().map(|k| describe(k, &by_key)).collect(),
        );
    }
    if !skip(
        "APP == approved, not queued",
        &["APP", "MINE_APPROVED", "MINE_QUEUED"],
    ) {
        let want = &set("MINE_APPROVED") - &set("MINE_QUEUED");
        report.check_eq(
            "APP == MINE_APPROVED \\ MINE_QUEUED",
            &want,
            &set("APP"),
            &by_key,
        );
    }
    if !skip(
        "WR == unreviewed, not queued",
        &["WR", "MINE_NONE", "MINE_QUEUED"],
    ) {
        let want = &set("MINE_NONE") - &set("MINE_QUEUED");
        report.check_eq("WR == MINE_NONE \\ MINE_QUEUED", &want, &set("WR"), &by_key);
    }
    if !skip("MRG == queued", &["MRG", "MINE_QUEUED"]) {
        let want = nondraft(&hits["MINE_QUEUED"]);
        report.check_eq("MRG == nondraft(MINE_QUEUED)", &want, &set("MRG"), &by_key);
    }
    if !skip("DR == drafts", &["DR", "MINE"]) {
        let want = draft(&hits["MINE"]);
        report.check_eq("DR == draft(MINE)", &want, &set("DR"), &by_key);
    }
    if !skip("NR/DNR split USER_REQ by draft", &["NR", "DNR", "USER_REQ"]) {
        report.check_eq(
            "NR == nondraft(USER_REQ)",
            &nondraft(&hits["USER_REQ"]),
            &set("NR"),
            &by_key,
        );
        report.check_eq(
            "DNR == draft(USER_REQ)",
            &draft(&hits["USER_REQ"]),
            &set("DNR"),
            &by_key,
        );
    }
    if !skip(
        "NTR == team requests minus user requests",
        &["NTR", "TEAM_REQ", "USER_REQ"],
    ) {
        let want = &nondraft(&hits["TEAM_REQ"]) - &set("USER_REQ");
        report.check_eq(
            "NTR == nondraft(TEAM_REQ) \\ USER_REQ",
            &want,
            &set("NTR"),
            &by_key,
        );
    }
    if !skip(
        "WA == reviewed, awaiting author",
        &["WA", "REVIEWED", "USER_REQ", "REVIEWED_QUEUED", "MINE"],
    ) {
        let mut want = nondraft(&hits["REVIEWED"]);
        for l in ["USER_REQ", "REVIEWED_QUEUED"] {
            want = &want - &set(l);
        }
        report.check_eq(
            "WA == nondraft(REVIEWED) \\ USER_REQ \\ REVIEWED_QUEUED",
            &want,
            &set("WA"),
            &by_key,
        );
        let overlap = &set("WA") & &set("MINE");
        report.check(
            overlap.is_empty(),
            "WA ∩ MINE == ∅",
            overlap.iter().map(|k| describe(k, &by_key)).collect(),
        );
    }

    if args.repos.len() >= 2 {
        let labels: Vec<String> = args.repos.iter().map(|r| format!("MINE_{r}")).collect();
        let mut refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        refs.push("MINE");
        if !skip("union of MINE_<repo> == MINE", &refs) {
            let union: BTreeSet<PrKey> = refs
                .iter()
                .filter(|l| **l != "MINE")
                .flat_map(|l| set(l))
                .collect();
            report.check_eq(
                "union of MINE_<repo> == MINE",
                &union,
                &set("MINE"),
                &by_key,
            );
        }
    } else {
        println!("SKIP union of MINE_<repo> == MINE (needs at least two --repo)");
    }

    // Each inbox's hits must be open PRs in a --repo, matching its
    // draft: token, newest first.
    let repo_set: BTreeSet<&str> = args.repos.iter().map(String::as_str).collect();
    for (short, (name, filter)) in INBOX_SHORT.iter().zip(inboxes::DEFAULT_INBOXES.iter()) {
        if !skip(&format!("{name} hits are in scope and sorted"), &[short]) {
            let found = &hits[*short];
            let want_draft = filter
                .split_whitespace()
                .find_map(|t| t.strip_prefix("draft:"))
                .map(|v| v == "true");
            let mut problems: Vec<String> = Vec::new();
            for h in found {
                if h.state != "OPEN" {
                    problems.push(format!("{} is {}", describe(&h.key, &by_key), h.state));
                }
                if want_draft.is_some_and(|d| h.is_draft != d) {
                    problems.push(format!(
                        "{} has wrong draft flag",
                        describe(&h.key, &by_key)
                    ));
                }
                if !repo_set.contains(h.key.repo.as_str()) {
                    problems.push(format!("{} is not in a --repo", describe(&h.key, &by_key)));
                }
            }
            for w in found.windows(2) {
                if w[0].updated_at < w[1].updated_at {
                    problems.push(format!("{} is out of order", describe(&w[1].key, &by_key)));
                }
            }
            report.check(
                problems.is_empty(),
                &format!("{name} hits are in scope and sorted"),
                problems,
            );
        }
    }

    println!("---\n{} passed, {} failed", report.passed, report.failed);
    Ok(if report.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
