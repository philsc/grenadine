//! The inboxes a new database starts with.

/// `(name, filter)` pairs, in display order.
pub const DEFAULT_INBOXES: &[(&str, &str)] = &[
    (
        "Needs your review",
        "state:open archived:false draft:false user-review-requested:@me sort:updated-desc",
    ),
    (
        "Needs your teams' review",
        "state:open archived:false draft:false team-review-requested-user:@me -user-review-requested:@me sort:updated-desc",
    ),
    (
        "Returned to you",
        "state:open archived:false draft:false author:@me -is:queued ((-review:approved -review:none) OR review:changes_requested) sort:updated-desc",
    ),
    (
        "Approved",
        "state:open archived:false draft:false author:@me -is:queued review:approved sort:updated-desc",
    ),
    (
        "Waiting for reviewers",
        "state:open archived:false draft:false author:@me -is:queued review:none sort:updated-desc",
    ),
    (
        "Drafts",
        "state:open archived:false draft:true author:@me sort:updated-desc",
    ),
    (
        "Merging",
        "state:open archived:false draft:false author:@me is:queued sort:updated-desc",
    ),
    (
        "Drafts needing your review",
        "state:open archived:false draft:true user-review-requested:@me sort:updated-desc",
    ),
    (
        "Waiting for authors",
        "state:open archived:false draft:false reviewed-by:@me -author:@me -user-review-requested:@me -is:queued sort:updated-desc",
    ),
];
