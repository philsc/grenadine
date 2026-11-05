//! The inboxes a new database starts with.

/// One default inbox.
pub struct DefaultInbox {
    pub name: &'static str,
    /// The GitHub search filter as the user would write it; `gr:`
    /// qualifiers are grenadine's own.
    pub filter: &'static str,
    /// Whether its PRs count in the top bar's total.
    pub count_in_total: bool,
    /// Whether the polls sync its PRs. One without it is listed from its
    /// search results, and its PRs sync when a page watches them.
    pub sync_in_background: bool,
}

/// The default inboxes, in display order. The same list is in the
/// README's "Default inboxes" table.
pub const DEFAULT_INBOXES: &[DefaultInbox] = &[
    DefaultInbox {
        name: "Needs your review",
        filter: "state:open archived:false draft:false -author:@me -is:queued user-review-requested:@me sort:updated-desc",
        count_in_total: true,
        sync_in_background: true,
    },
    DefaultInbox {
        name: "Needs your teams' review",
        filter: "state:open archived:false draft:false team-review-requested-user:@me -user-review-requested:@me sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "Returned to you",
        filter: "state:open archived:false draft:false author:@me -is:queued ((gr:review:active -review:approved) OR gr:changes-requested:unaddressed) gr:sort:title-asc",
        count_in_total: true,
        sync_in_background: true,
    },
    DefaultInbox {
        name: "Approved",
        filter: "state:open archived:false draft:false author:@me -is:queued review:approved gr:approvals:>0 -gr:changes-requested:unaddressed sort:updated-desc",
        count_in_total: true,
        sync_in_background: true,
    },
    DefaultInbox {
        name: "Merging",
        filter: "state:open archived:false draft:false author:@me is:queued sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "Drafts",
        filter: "state:open archived:false draft:true author:@me sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "Drafts from others",
        filter: "state:open archived:false draft:true -author:@me user-review-requested:@me gr:sort:title-asc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "Waiting for reviewers",
        filter: "state:open archived:false draft:false author:@me -is:queued -gr:review:active -review:approved sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "All my open PRs",
        filter: "state:open archived:false author:@me sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "Waiting for author",
        filter: "state:open archived:false draft:false reviewed-by:@me -author:@me -user-review-requested:@me -is:queued sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "Merged by me",
        filter: "state:closed archived:false author:@me sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "Merged by others",
        filter: "state:closed archived:false -author:@me (reviewed-by:@me OR user-review-requested:@me) sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "All open PRs",
        filter: "state:open archived:false -is:queued sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
    DefaultInbox {
        name: "Unmerged PRs on mine",
        filter: "state:closed archived:false author:@me is:unmerged sort:updated-desc",
        count_in_total: false,
        sync_in_background: false,
    },
];
