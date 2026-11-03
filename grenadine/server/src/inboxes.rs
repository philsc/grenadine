//! The inboxes a new database starts with.

/// One default inbox.
pub struct DefaultInbox {
    pub name: &'static str,
    /// The GitHub search filter as the user would write it.
    pub filter: &'static str,
    /// Whether its PRs count in the top bar's total.
    pub count_in_total: bool,
}

/// The default inboxes, in display order.
pub const DEFAULT_INBOXES: &[DefaultInbox] = &[
    DefaultInbox {
        name: "Needs your review",
        filter: "state:open archived:false draft:false user-review-requested:@me sort:updated-desc",
        count_in_total: true,
    },
    DefaultInbox {
        name: "Needs your teams' review",
        filter: "state:open archived:false draft:false team-review-requested-user:@me -user-review-requested:@me sort:updated-desc",
        count_in_total: false,
    },
    DefaultInbox {
        name: "Returned to you",
        filter: "state:open archived:false draft:false author:@me -is:queued ((-review:approved -review:none) OR review:changes_requested) sort:updated-desc",
        count_in_total: true,
    },
    DefaultInbox {
        name: "Approved",
        filter: "state:open archived:false draft:false author:@me -is:queued review:approved sort:updated-desc",
        count_in_total: true,
    },
    DefaultInbox {
        name: "Waiting for reviewers",
        filter: "state:open archived:false draft:false author:@me -is:queued review:none sort:updated-desc",
        count_in_total: false,
    },
    DefaultInbox {
        name: "Drafts",
        filter: "state:open archived:false draft:true author:@me sort:updated-desc",
        count_in_total: false,
    },
    DefaultInbox {
        name: "Merging",
        filter: "state:open archived:false draft:false author:@me is:queued sort:updated-desc",
        count_in_total: false,
    },
    DefaultInbox {
        name: "Drafts needing your review",
        filter: "state:open archived:false draft:true user-review-requested:@me sort:updated-desc",
        count_in_total: false,
    },
    DefaultInbox {
        name: "Waiting for authors",
        filter: "state:open archived:false draft:false reviewed-by:@me -author:@me -user-review-requested:@me -is:queued sort:updated-desc",
        count_in_total: false,
    },
];
