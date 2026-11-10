//! Inbox filters with `gr:` qualifiers, as documented in the top-level
//! README: a GitHub search string that grenadine evaluates itself when it
//! contains any `gr:` token. `parse` builds a `Filter`, `search_terms`
//! gives the widened query that goes to GitHub and `matches` evaluates the
//! whole expression on one search hit.

/// A review on a PR, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewFact {
    pub author: String,
    /// A GitHub bot (an app) wrote the review.
    pub bot: bool,
    /// APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED or PENDING.
    pub state: String,
}

/// Everything `matches` needs to know about one PR.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrFacts {
    pub title: String,
    pub author: String,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    pub is_draft: bool,
    /// Its repository is archived.
    pub archived: bool,
    /// In a merge queue.
    pub queued: bool,
    /// GitHub's review decision: APPROVED, CHANGES_REQUESTED or
    /// REVIEW_REQUIRED.
    pub review_decision: Option<String>,
    pub reviews: Vec<ReviewFact>,
    /// Logins of the users asked to review; team requests aren't in it.
    pub user_review_requests: Vec<String>,
}

/// The direction a `gr:sort:` qualifier asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleSort {
    Asc,
    Desc,
}

/// What kind of term one token is, decided while parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// A bare word or a qualifier only GitHub understands. Grenadine
    /// can't evaluate it, so it must be a top-level term and counts as
    /// matched: GitHub already filtered on it.
    SearchOnly,
    /// A GitHub qualifier grenadine evaluates itself; inside an `OR`
    /// GitHub's verdict can't be trusted.
    Local,
    /// A `gr:` qualifier grenadine evaluates itself.
    Gr(Gr),
    /// `gr:sort:title-asc` or `gr:sort:title-desc`; not a condition.
    TitleSort(TitleSort),
}

/// A parsed `gr:` qualifier other than `gr:sort:`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Gr {
    ReviewActive,
    /// An active review written by a person, not a bot.
    ReviewActiveHuman,
    /// An active human review whose reviewer's standing verdict isn't
    /// APPROVED.
    ReviewUnapprovedHuman,
    ChangesUnaddressed,
    Approvals(u64),
    ApprovedBy(String),
    Rerequested(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Term {
    /// The token as written, without a leading `-`.
    text: String,
    negated: bool,
    kind: Kind,
}

/// The filter as an expression tree. `True` is what a `gr:` literal
/// widens to for the GitHub search.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Expr {
    True,
    Term(Term),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// A parsed filter: the expression, the `sort:` tokens for GitHub and
/// the optional local title sort.
#[derive(Debug)]
pub struct Filter {
    expr: Expr,
    sorts: Vec<String>,
    title_sort: Option<TitleSort>,
}

fn tokenize(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '(' | ')' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(c.to_string());
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            // A quoted segment stays inside its token, as in
            // `label:"needs review"`.
            '"' => {
                cur.push('"');
                for d in chars.by_ref() {
                    cur.push(d);
                    if d == '"' {
                        break;
                    }
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Whether the filter contains any `gr:` token, negated or not. Only
/// then does grenadine parse and evaluate it.
pub fn has_local_terms(filter: &str) -> bool {
    tokenize(filter)
        .iter()
        .any(|t| t.starts_with("gr:") || t.starts_with("-gr:"))
}

/// The GitHub qualifiers grenadine evaluates itself. Anything else with
/// a colon, and bare words, are search-only.
fn is_local(term: &str) -> bool {
    let (key, value) = match term.split_once(':') {
        Some(kv) => kv,
        None => return false,
    };
    match key {
        "is" => matches!(
            value,
            "pr" | "open" | "closed" | "merged" | "unmerged" | "draft" | "queued"
        ),
        "state" => matches!(value, "open" | "closed" | "merged"),
        "draft" | "archived" => matches!(value, "true" | "false"),
        "author" | "user-review-requested" | "reviewed-by" => true,
        "review" => matches!(value, "approved" | "changes_requested" | "none"),
        _ => false,
    }
}

fn classify(term: &str) -> Result<Kind, String> {
    let Some(rest) = term.strip_prefix("gr:") else {
        return Ok(if is_local(term) {
            Kind::Local
        } else {
            Kind::SearchOnly
        });
    };
    let (name, value) = rest.split_once(':').unwrap_or((rest, ""));
    Ok(match name {
        "sort" => match value {
            "title-asc" => Kind::TitleSort(TitleSort::Asc),
            "title-desc" => Kind::TitleSort(TitleSort::Desc),
            _ => return Err("gr:sort: knows only title-asc and title-desc".into()),
        },
        "review" if value == "active" => Kind::Gr(Gr::ReviewActive),
        "review" if value == "active-human" => Kind::Gr(Gr::ReviewActiveHuman),
        "review" if value == "unapproved-human" => Kind::Gr(Gr::ReviewUnapprovedHuman),
        "changes-requested" if value == "unaddressed" => Kind::Gr(Gr::ChangesUnaddressed),
        "approvals" => match value.strip_prefix('>').and_then(|n| n.parse::<u64>().ok()) {
            Some(n) => Kind::Gr(Gr::Approvals(n)),
            None => return Err("gr:approvals: needs a > followed by a number".into()),
        },
        "approved-by" if !value.is_empty() => Kind::Gr(Gr::ApprovedBy(value.to_owned())),
        "rerequested" if !value.is_empty() => Kind::Gr(Gr::Rerequested(value.to_owned())),
        _ => return Err(format!("unknown grenadine qualifier gr:{rest}")),
    })
}

/// Parses `filter`. The grammar matches GitHub's: AND by juxtaposition
/// binds tighter than `OR`, parentheses group, `-` negates one token.
pub fn parse(filter: &str) -> Result<Filter, String> {
    // `sort:` tokens go to GitHub wherever they stand, so they come out
    // of the token stream before parsing, as on GitHub.
    let mut sorts = Vec::new();
    let mut tokens = Vec::new();
    for t in tokenize(filter) {
        if t.starts_with("sort:") {
            sorts.push(t);
        } else {
            tokens.push(t);
        }
    }
    let mut pos = 0;

    fn peek(tokens: &[String], pos: usize) -> Option<&str> {
        tokens.get(pos).map(String::as_str)
    }

    fn or_expr(tokens: &[String], pos: &mut usize) -> Result<Expr, String> {
        let mut terms = vec![and_expr(tokens, pos)?];
        while peek(tokens, *pos) == Some("OR") {
            *pos += 1;
            terms.push(and_expr(tokens, pos)?);
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Expr::Or(terms)
        })
    }

    fn and_expr(tokens: &[String], pos: &mut usize) -> Result<Expr, String> {
        let mut terms = Vec::new();
        while !matches!(peek(tokens, *pos), None | Some(")") | Some("OR")) {
            terms.push(unary(tokens, pos)?);
        }
        if terms.is_empty() {
            return Err("expected a term".into());
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Expr::And(terms)
        })
    }

    fn unary(tokens: &[String], pos: &mut usize) -> Result<Expr, String> {
        let tok = tokens[*pos].clone();
        *pos += 1;
        if tok == "(" {
            let inner = or_expr(tokens, pos)?;
            if peek(tokens, *pos) != Some(")") {
                return Err("unbalanced parentheses".into());
            }
            *pos += 1;
            return Ok(inner);
        }
        let (text, negated) = match tok.strip_prefix('-').filter(|_| tok.len() > 1) {
            Some(text) => (text.to_owned(), true),
            None => (tok, false),
        };
        Ok(Expr::Term(Term {
            kind: classify(&text)?,
            text,
            negated,
        }))
    }

    let expr = if tokens.is_empty() {
        Expr::True
    } else {
        let expr = or_expr(&tokens, &mut pos)?;
        if pos != tokens.len() {
            return Err(format!("unexpected {:?}", tokens[pos]));
        }
        expr
    };

    // Search-only terms and `gr:sort:` are only valid as top-level
    // conjuncts: the root AND's children, or the root itself when it is
    // a single term.
    let mut title_sort = None;
    fn check(e: &Expr, conjunct: bool, title_sort: &mut Option<TitleSort>) -> Result<(), String> {
        match e {
            Expr::Term(Term {
                kind: Kind::SearchOnly,
                text,
                ..
            }) if !conjunct => Err(format!(
                "{text} is only evaluated by GitHub, so it must be a top-level term"
            )),
            Expr::Term(Term {
                kind: Kind::TitleSort(sort),
                text,
                negated,
            }) => {
                if *negated {
                    Err(format!("{text} can't be negated"))
                } else if !conjunct {
                    Err(format!("{text} must be a top-level term"))
                } else if title_sort.replace(*sort).is_some() {
                    Err("only one gr:sort: term is allowed".into())
                } else {
                    Ok(())
                }
            }
            Expr::And(children) | Expr::Or(children) => {
                for c in children {
                    check(c, false, title_sort)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    match &expr {
        Expr::And(children) => {
            for c in children {
                check(c, true, &mut title_sort)?;
            }
        }
        _ => check(&expr, true, &mut title_sort)?,
    }

    Ok(Filter {
        expr,
        sorts,
        title_sort,
    })
}

/// The `gr:` literal terms of `e` replaced by TRUE, folded: an AND
/// drops TRUE children, an OR with a TRUE child is TRUE.
fn widen(e: &Expr) -> Expr {
    match e {
        Expr::Term(Term {
            kind: Kind::Gr(_) | Kind::TitleSort(_),
            ..
        }) => Expr::True,
        Expr::And(children) => {
            let children: Vec<Expr> = children
                .iter()
                .map(widen)
                .filter(|e| *e != Expr::True)
                .collect();
            match children.len() {
                0 => Expr::True,
                1 => children.into_iter().next().unwrap(),
                _ => Expr::And(children),
            }
        }
        Expr::Or(children) => {
            let children: Vec<Expr> = children.iter().map(widen).collect();
            if children.contains(&Expr::True) {
                return Expr::True;
            }
            match children.len() {
                1 => children.into_iter().next().unwrap(),
                _ => Expr::Or(children),
            }
        }
        _ => e.clone(),
    }
}

/// The terms of `e` as a query string; ORs nested in an AND get
/// parentheses, since AND binds tighter.
fn serialize(e: &Expr) -> String {
    fn in_and(e: &Expr) -> String {
        match e {
            Expr::Or(_) => format!("({})", serialize(e)),
            _ => serialize(e),
        }
    }
    match e {
        Expr::True => String::new(),
        Expr::Term(Term { text, negated, .. }) => {
            if *negated {
                format!("-{text}")
            } else {
                text.clone()
            }
        }
        Expr::And(children) => children.iter().map(in_and).collect::<Vec<_>>().join(" "),
        Expr::Or(children) => children
            .iter()
            .map(serialize)
            .collect::<Vec<_>>()
            .join(" OR "),
    }
}

/// The resolved login of a qualifier value: `@me` is the token's owner.
fn who<'a>(value: &'a str, me: &'a str) -> &'a str {
    if value == "@me" { me } else { value }
}

fn is_user(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn requested(pr: &PrFacts, user: &str) -> bool {
    pr.user_review_requests.iter().any(|u| is_user(u, user))
}

/// Each reviewer's standing verdict: their latest review that is
/// APPROVED, CHANGES_REQUESTED or DISMISSED, where DISMISSED means no
/// verdict. COMMENTED and PENDING reviews don't change it.
fn verdict<'a>(pr: &'a PrFacts, user: &str) -> Option<&'a str> {
    pr.reviews
        .iter()
        .rev()
        .find(|r| {
            is_user(&r.author, user)
                && matches!(
                    r.state.as_str(),
                    "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
                )
        })
        .and_then(|r| (r.state != "DISMISSED").then_some(r.state.as_str()))
}

fn eval_local(text: &str, pr: &PrFacts, me: &str) -> bool {
    let Some((key, value)) = text.split_once(':') else {
        return true;
    };
    match (key, value) {
        ("is", "pr") => true,
        ("is", "open") | ("state", "open") => pr.state == "OPEN",
        ("is", "closed") | ("state", "closed") => pr.state != "OPEN",
        ("is", "merged") | ("state", "merged") => pr.state == "MERGED",
        ("is", "unmerged") => pr.state != "MERGED",
        ("is", "draft") => pr.is_draft,
        ("is", "queued") => pr.queued,
        ("draft", v) => pr.is_draft == (v == "true"),
        ("archived", v) => pr.archived == (v == "true"),
        ("author", u) => is_user(&pr.author, who(u, me)),
        ("user-review-requested", u) => requested(pr, who(u, me)),
        ("reviewed-by", u) => {
            let u = who(u, me);
            pr.reviews
                .iter()
                .any(|r| is_user(&r.author, u) && r.state != "PENDING")
        }
        ("review", "approved") => pr.review_decision.as_deref() == Some("APPROVED"),
        ("review", "changes_requested") => {
            pr.review_decision.as_deref() == Some("CHANGES_REQUESTED")
        }
        ("review", "none") => !pr
            .reviews
            .iter()
            .any(|r| matches!(r.state.as_str(), "APPROVED" | "CHANGES_REQUESTED")),
        _ => true,
    }
}

/// Whether `r` counts towards the PR's review state: submitted, not the
/// author's own and not made moot by a re-request.
fn active_review(pr: &PrFacts, r: &ReviewFact) -> bool {
    !matches!(r.state.as_str(), "DISMISSED" | "PENDING")
        && !is_user(&r.author, &pr.author)
        && !requested(pr, &r.author)
}

fn eval_gr(gr: &Gr, pr: &PrFacts, me: &str) -> bool {
    match gr {
        Gr::ReviewActive => pr.reviews.iter().any(|r| active_review(pr, r)),
        Gr::ReviewActiveHuman => pr.reviews.iter().any(|r| active_review(pr, r) && !r.bot),
        Gr::ReviewUnapprovedHuman => pr
            .reviews
            .iter()
            .any(|r| active_review(pr, r) && !r.bot && verdict(pr, &r.author) != Some("APPROVED")),
        Gr::ChangesUnaddressed => pr
            .reviews
            .iter()
            .map(|r| r.author.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .any(|u| verdict(pr, u) == Some("CHANGES_REQUESTED") && !requested(pr, u)),
        Gr::Approvals(n) => {
            pr.reviews
                .iter()
                .map(|r| r.author.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .filter(|u| verdict(pr, u) == Some("APPROVED"))
                .count()
                > *n as usize
        }
        Gr::ApprovedBy(u) => verdict(pr, who(u, me)) == Some("APPROVED"),
        Gr::Rerequested(u) => {
            let u = who(u, me);
            requested(pr, u)
                && pr
                    .reviews
                    .iter()
                    .any(|r| is_user(&r.author, u) && r.state != "PENDING")
        }
    }
}

fn eval(e: &Expr, pr: &PrFacts, me: &str) -> bool {
    match e {
        Expr::True => true,
        Expr::Term(Term {
            text,
            negated,
            kind,
        }) => {
            let value = match kind {
                Kind::SearchOnly | Kind::TitleSort(_) => true,
                Kind::Local => eval_local(text, pr, me),
                Kind::Gr(gr) => eval_gr(gr, pr, me),
            };
            value != *negated
        }
        Expr::And(children) => children.iter().all(|e| eval(e, pr, me)),
        Expr::Or(children) => children.iter().any(|e| eval(e, pr, me)),
    }
}

impl Filter {
    /// The widened query terms plus the `sort:` tokens, for
    /// `github::search_query`.
    pub fn search_terms(&self) -> String {
        let terms = serialize(&widen(&self.expr));
        if self.sorts.is_empty() {
            terms
        } else if terms.is_empty() {
            self.sorts.join(" ")
        } else {
            format!("{terms} {}", self.sorts.join(" "))
        }
    }

    /// Whether `pr` matches the whole filter. Search-only terms count
    /// as matched because GitHub already filtered on them.
    pub fn matches(&self, pr: &PrFacts, me: &str) -> bool {
        eval(&self.expr, pr, me)
    }

    /// The local title sort `gr:sort:` asked for.
    pub fn title_sort(&self) -> Option<TitleSort> {
        self.title_sort
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> PrFacts {
        PrFacts {
            title: "t".into(),
            author: "me".into(),
            state: "OPEN".into(),
            is_draft: false,
            archived: false,
            queued: false,
            review_decision: None,
            reviews: vec![],
            user_review_requests: vec![],
        }
    }

    fn review(author: &str, state: &str) -> ReviewFact {
        ReviewFact {
            author: author.into(),
            bot: false,
            state: state.into(),
        }
    }

    fn bot_review(author: &str, state: &str) -> ReviewFact {
        ReviewFact {
            bot: true,
            ..review(author, state)
        }
    }

    fn parses(filter: &str) -> Filter {
        parse(filter).unwrap()
    }

    #[test]
    fn tokenizing() {
        assert_eq!(
            tokenize("a (b OR -c) label:\"needs review\""),
            ["a", "(", "b", "OR", "-c", ")", "label:\"needs review\""]
        );
        assert_eq!(tokenize("x(y"), ["x", "(", "y"]);
    }

    #[test]
    fn has_local_terms() {
        assert!(!super::has_local_terms("author:@me (a OR b)"));
        assert!(super::has_local_terms("gr:review:active"));
        assert!(super::has_local_terms("(-gr:review:active)"));
    }

    #[test]
    fn precedence() {
        let f = parses("author:a author:b OR is:draft");
        assert!(matches!(f.expr, Expr::Or(_)));
        // The first two AND first, then OR with is:draft.
        let mut pr = facts();
        // Local terms only, so evaluation is meaningful.
        assert!(parses("is:open author:me OR draft:true").matches(&pr, "me"));
        assert!(!parses("is:open (author:other OR draft:true)").matches(&pr, "me"));
        pr.is_draft = true;
        assert!(parses("is:open (author:other OR draft:true)").matches(&pr, "me"));
    }

    #[test]
    fn parse_errors() {
        assert_eq!(parse("(a:b").unwrap_err(), "unbalanced parentheses");
        assert_eq!(parse("a:b)").unwrap_err(), "unexpected \")\"");
        assert!(parse("()").is_err());
        assert!(parse("a:b ()").is_err());
        assert!(parse("a:b OR").is_err());
        assert_eq!(
            parse("gr:bogus:x").unwrap_err(),
            "unknown grenadine qualifier gr:bogus:x"
        );
        assert_eq!(
            parse("gr:bogus").unwrap_err(),
            "unknown grenadine qualifier gr:bogus"
        );
        assert!(parse("gr:approvals:x").is_err());
        assert!(parse("gr:approvals:>-1").is_err());
        assert_eq!(
            parse("-gr:sort:title-asc").unwrap_err(),
            "gr:sort:title-asc can't be negated"
        );
        assert!(parse("a:b (gr:sort:title-asc OR is:open)").is_err());
        assert!(parse("gr:sort:title-asc gr:sort:title-desc").is_err());
        assert!(parse("is:open (label:x OR gr:review:active)").is_err());
        assert!(parse("label:x OR gr:review:active").is_err());
        assert!(parse("gr:review:active (is:open label:x)").is_err());
        // Bare words are search-only too.
        assert!(parse("is:open (word OR gr:review:active)").is_err());
        // A negated search-only term is fine at top level.
        assert!(parse("is:open -label:x gr:review:active").is_ok());
        // Search-only terms in a parenthesised AND are still not
        // top-level.
        assert!(parse("is:open (label:x label:y) gr:review:active").is_err());
    }

    #[test]
    fn widening() {
        let w = |f: &str| parses(f).search_terms();
        assert_eq!(w("gr:review:active"), "");
        assert_eq!(w("gr:review:active-human"), "");
        assert_eq!(w("gr:review:unapproved-human"), "");
        assert_eq!(w("is:open gr:review:active"), "is:open");
        assert_eq!(w("-gr:review:active is:open"), "is:open");
        assert_eq!(
            w("is:open (gr:review:active OR gr:approved-by:x)"),
            "is:open"
        );
        assert_eq!(w("is:open (author:me OR gr:review:active)"), "is:open");
        // An OR with a gr: literal child widens to TRUE wholesale.
        assert_eq!(
            w(
                "author:me ((gr:review:active -review:approved) OR gr:changes-requested:unaddressed)"
            ),
            "author:me"
        );
        assert_eq!(w("gr:review:active sort:updated-desc"), "sort:updated-desc");
        assert_eq!(
            w("is:open gr:review:active sort:updated-asc"),
            "is:open sort:updated-asc"
        );
        // Search-only terms stay verbatim.
        assert_eq!(
            w("draft:false label:\"needs review\" -updated:<2020 gr:review:active"),
            "draft:false label:\"needs review\" -updated:<2020"
        );
    }

    #[test]
    fn widening_of_the_defaults() {
        let cases = [
            (
                "state:open archived:false draft:false -author:@me -is:queued user-review-requested:@me sort:updated-desc",
                "state:open archived:false draft:false -author:@me -is:queued user-review-requested:@me sort:updated-desc",
            ),
            (
                "state:open archived:false draft:false author:@me -is:queued ((gr:review:unapproved-human -review:approved) OR gr:changes-requested:unaddressed) gr:sort:title-asc",
                "state:open archived:false draft:false author:@me -is:queued",
            ),
            (
                "state:open archived:false draft:false author:@me -is:queued review:approved gr:approvals:>0 -gr:changes-requested:unaddressed sort:updated-desc",
                "state:open archived:false draft:false author:@me -is:queued review:approved sort:updated-desc",
            ),
            (
                "state:open archived:false draft:true -author:@me user-review-requested:@me gr:sort:title-asc",
                "state:open archived:false draft:true -author:@me user-review-requested:@me",
            ),
            (
                "state:open archived:false draft:false author:@me -is:queued -gr:review:unapproved-human -gr:changes-requested:unaddressed -review:approved sort:updated-desc",
                "state:open archived:false draft:false author:@me -is:queued -review:approved sort:updated-desc",
            ),
        ];
        for (filter, want) in cases {
            assert_eq!(parses(filter).search_terms(), want, "{filter}");
        }
    }

    #[test]
    fn bob_scenario() {
        // Bob requests changes; the author fixes and re-requests him.
        let f = parses("gr:review:active OR gr:changes-requested:unaddressed");
        let mut pr = facts();
        pr.reviews = vec![review("bob", "CHANGES_REQUESTED")];
        assert!(f.matches(&pr, "me"));
        pr.user_review_requests = vec!["bob".into()];
        // Re-requested, his review is no longer active nor unaddressed.
        assert!(!f.matches(&pr, "me"));
        // His standing verdict is still CHANGES_REQUESTED.
        assert!(parses("-gr:changes-requested:unaddressed").matches(&pr, "me"));
        assert_eq!(verdict(&pr, "bob"), Some("CHANGES_REQUESTED"));
    }

    #[test]
    fn dismissed_review() {
        let mut pr = facts();
        pr.reviews = vec![review("bob", "APPROVED"), review("bob", "DISMISSED")];
        assert_eq!(verdict(&pr, "bob"), None);
        assert!(!parses("gr:approved-by:bob").matches(&pr, "me"));
        // The APPROVED review is itself not DISMISSED, so the PR still
        // has an active review.
        assert!(parses("gr:review:active").matches(&pr, "me"));
        // But reviewed-by counts dismissed reviews.
        assert!(parses("reviewed-by:bob").matches(&pr, "me"));
        // review:none looks at the reviews as they are, and an APPROVED
        // one is still there.
        assert!(!parses("review:none").matches(&pr, "me"));

        // An earlier COMMENTED review is active even with the verdict
        // dismissed.
        pr.reviews = vec![
            review("bob", "COMMENTED"),
            review("bob", "CHANGES_REQUESTED"),
            review("bob", "DISMISSED"),
        ];
        assert!(parses("gr:review:active").matches(&pr, "me"));
        // His standing verdict is still gone.
        assert_eq!(verdict(&pr, "bob"), None);
        // And only the DISMISSED state fails to be active.
        pr.reviews = vec![review("bob", "DISMISSED")];
        assert!(!parses("gr:review:active").matches(&pr, "me"));
    }

    #[test]
    fn authors_own_comment_is_not_active() {
        let mut pr = facts();
        pr.reviews = vec![review("me", "COMMENTED")];
        assert!(!parses("gr:review:active").matches(&pr, "me"));
        pr.reviews.push(review("other", "COMMENTED"));
        assert!(parses("gr:review:active").matches(&pr, "me"));
    }

    #[test]
    fn bot_reviews() {
        let mut pr = facts();
        pr.reviews = vec![bot_review("ci-bot", "COMMENTED")];
        assert!(parses("gr:review:active").matches(&pr, "me"));
        assert!(!parses("gr:review:active-human").matches(&pr, "me"));
        pr.reviews = vec![review("bob", "COMMENTED")];
        assert!(parses("gr:review:active").matches(&pr, "me"));
        assert!(parses("gr:review:active-human").matches(&pr, "me"));
        // The author's own comment and a re-requested reviewer's still
        // aren't active.
        pr.reviews = vec![review("me", "COMMENTED"), review("bob", "COMMENTED")];
        pr.user_review_requests = vec!["bob".into()];
        assert!(!parses("gr:review:active-human").matches(&pr, "me"));
    }

    #[test]
    fn unapproved_human_review() {
        let f = parses("gr:review:unapproved-human");
        let m = |pr: &PrFacts| f.matches(pr, "me");
        let mut pr = facts();
        // An approval is not an unapproved review, even when a later
        // comment-only review by the same reviewer is active.
        pr.reviews = vec![review("bob", "APPROVED")];
        assert!(!m(&pr));
        pr.reviews = vec![review("bob", "APPROVED"), review("bob", "COMMENTED")];
        assert!(!m(&pr));
        // A comment-only review carries no verdict, so it is unapproved.
        pr.reviews = vec![review("bob", "COMMENTED")];
        assert!(m(&pr));
        // A bot's comment doesn't count.
        pr.reviews = vec![bot_review("ci-bot", "COMMENTED")];
        assert!(!m(&pr));
        // A change request is unapproved.
        pr.reviews = vec![review("bob", "CHANGES_REQUESTED")];
        assert!(m(&pr));
        // A re-requested reviewer's comment isn't active.
        pr.user_review_requests = vec!["bob".into()];
        assert!(!m(&pr));
        pr.user_review_requests = vec![];
        // The author's own comment isn't active either.
        pr.reviews = vec![review("me", "COMMENTED")];
        assert!(!m(&pr));
        // One approver plus another reviewer's comment is unapproved.
        pr.reviews = vec![review("bob", "APPROVED"), review("ann", "COMMENTED")];
        assert!(m(&pr));
    }

    #[test]
    fn rerequested() {
        let mut pr = facts();
        pr.user_review_requests = vec!["bob".into()];
        // Requested but never reviewed: not re-requested.
        assert!(!parses("gr:rerequested:bob").matches(&pr, "me"));
        pr.reviews = vec![review("bob", "COMMENTED")];
        assert!(parses("gr:rerequested:bob").matches(&pr, "me"));
        assert!(parses("gr:rerequested:@me").matches(&pr, "bob"));
    }

    #[test]
    fn approvals_and_approved_by() {
        let mut pr = facts();
        pr.reviews = vec![
            review("a", "APPROVED"),
            review("b", "APPROVED"),
            review("b", "CHANGES_REQUESTED"),
        ];
        assert!(parses("gr:approvals:>0").matches(&pr, "me"));
        assert!(!parses("gr:approvals:>1").matches(&pr, "me"));
        assert!(parses("gr:approved-by:a").matches(&pr, "me"));
        assert!(!parses("gr:approved-by:b").matches(&pr, "me"));
        // Re-requesting an approver doesn't cancel the approval.
        pr.user_review_requests = vec!["a".into()];
        assert!(parses("gr:approvals:>0").matches(&pr, "me"));
        assert!(parses("gr:approved-by:a").matches(&pr, "me"));
        // Logins compare case-insensitively.
        assert!(parses("gr:approved-by:A").matches(&pr, "me"));
    }

    #[test]
    fn local_qualifiers() {
        let mut pr = facts();
        let m = |f: &str, pr: &PrFacts| parses(f).matches(pr, "me");
        assert!(m("is:pr", &pr) && m("is:open state:open", &pr));
        assert!(!m("is:closed", &pr) && m("is:unmerged", &pr));
        assert!(m("author:@me", &pr) && m("author:ME", &pr));
        assert!(!m("is:draft draft:true", &pr) && m("draft:false", &pr));
        assert!(m("archived:false", &pr) && !m("archived:true", &pr));
        assert!(!m("is:queued", &pr));
        assert!(m("review:none", &pr));
        assert!(m("-author:other", &pr));

        pr.state = "MERGED".into();
        assert!(m("is:closed state:closed", &pr));
        assert!(m("is:merged state:merged", &pr));
        assert!(!m("is:unmerged", &pr));

        let mut pr = facts();
        pr.user_review_requests = vec!["bob".into()];
        assert!(m("user-review-requested:bob", &pr));
        assert!(!m("user-review-requested:@me", &pr));
        pr.reviews = vec![review("me", "COMMENTED"), review("bob", "PENDING")];
        assert!(m("reviewed-by:me", &pr));
        assert!(!m("reviewed-by:bob", &pr));
        assert!(m("review:none", &pr));
        pr.review_decision = Some("APPROVED".into());
        assert!(m("review:approved", &pr));
        assert!(!m("review:changes_requested", &pr));
    }

    #[test]
    fn default_inboxes_partition() {
        // The Returned/Approved/Waiting/Merging filters over a table of
        // non-draft open authored PRs: exactly one holds each.
        let ret = parses(
            "state:open archived:false draft:false author:@me -is:queued ((gr:review:unapproved-human -review:approved) OR gr:changes-requested:unaddressed)",
        );
        let app = parses(
            "state:open archived:false draft:false author:@me -is:queued review:approved gr:approvals:>0 -gr:changes-requested:unaddressed",
        );
        let mrg = parses("state:open archived:false draft:false author:@me is:queued");
        let wr = parses(
            "state:open archived:false draft:false author:@me -is:queued -gr:review:unapproved-human -gr:changes-requested:unaddressed -review:approved",
        );
        let base = facts();
        let cases: Vec<(PrFacts, [bool; 4])> = vec![
            // No reviews: Waiting for reviewers.
            (base.clone(), [false, false, false, true]),
            // Comment-only review: an active review, so Returned.
            (
                PrFacts {
                    reviews: vec![review("bob", "COMMENTED")],
                    ..base.clone()
                },
                [true, false, false, false],
            ),
            // Changes requested: Returned.
            (
                PrFacts {
                    reviews: vec![review("bob", "CHANGES_REQUESTED")],
                    review_decision: Some("CHANGES_REQUESTED".into()),
                    ..base.clone()
                },
                [true, false, false, false],
            ),
            // Changes requested, then re-requested: Waiting.
            (
                PrFacts {
                    reviews: vec![review("bob", "CHANGES_REQUESTED")],
                    user_review_requests: vec!["bob".into()],
                    review_decision: Some("CHANGES_REQUESTED".into()),
                    ..base.clone()
                },
                [false, false, false, true],
            ),
            // A bot's comment-only review: not an active human review,
            // so still Waiting.
            (
                PrFacts {
                    reviews: vec![bot_review("ci-bot", "COMMENTED")],
                    ..base.clone()
                },
                [false, false, false, true],
            ),
            // A bot's change request: unaddressed, so Returned.
            (
                PrFacts {
                    reviews: vec![bot_review("ci-bot", "CHANGES_REQUESTED")],
                    review_decision: Some("CHANGES_REQUESTED".into()),
                    ..base.clone()
                },
                [true, false, false, false],
            ),
            // Approved: Approved.
            (
                PrFacts {
                    reviews: vec![review("bob", "APPROVED")],
                    review_decision: Some("APPROVED".into()),
                    ..base.clone()
                },
                [false, true, false, false],
            ),
            // Approved then re-requested: still Approved.
            (
                PrFacts {
                    reviews: vec![review("bob", "APPROVED")],
                    review_decision: Some("APPROVED".into()),
                    user_review_requests: vec!["bob".into()],
                    ..base.clone()
                },
                [false, true, false, false],
            ),
            // Approved with an unaddressed change request: Returned.
            (
                PrFacts {
                    reviews: vec![
                        review("bob", "APPROVED"),
                        review("ann", "CHANGES_REQUESTED"),
                    ],
                    review_decision: Some("CHANGES_REQUESTED".into()),
                    ..base.clone()
                },
                [true, false, false, false],
            ),
            // Approved, but branch protection still wants reviews: the
            // approval isn't a reason to return the PR, so Waiting.
            (
                PrFacts {
                    reviews: vec![review("bob", "APPROVED")],
                    review_decision: Some("REVIEW_REQUIRED".into()),
                    ..base.clone()
                },
                [false, false, false, true],
            ),
            // The same, plus another reviewer's comment: Returned.
            (
                PrFacts {
                    reviews: vec![review("bob", "APPROVED"), review("ann", "COMMENTED")],
                    review_decision: Some("REVIEW_REQUIRED".into()),
                    ..base.clone()
                },
                [true, false, false, false],
            ),
            // Queued: Merging.
            (
                PrFacts {
                    queued: true,
                    reviews: vec![review("bob", "APPROVED")],
                    review_decision: Some("APPROVED".into()),
                    ..base.clone()
                },
                [false, false, true, false],
            ),
        ];
        for (pr, want) in cases {
            let got = [
                ret.matches(&pr, "me"),
                app.matches(&pr, "me"),
                mrg.matches(&pr, "me"),
                wr.matches(&pr, "me"),
            ];
            assert_eq!(got, want, "{pr:?}");
        }
    }

    #[test]
    fn title_sort() {
        assert_eq!(parses("is:open").title_sort(), None);
        assert_eq!(
            parses("is:open gr:sort:title-asc").title_sort(),
            Some(TitleSort::Asc)
        );
        assert_eq!(
            parses("gr:sort:title-desc is:open").title_sort(),
            Some(TitleSort::Desc)
        );
    }
}
