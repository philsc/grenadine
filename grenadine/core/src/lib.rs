//! Code shared between the grenadine server and its web UI: the JSON API
//! types, the algorithm that splits a PR's push history into versions, and
//! the matcher that finds hunks introduced by a rebase.

pub mod api;
pub mod diff;
pub mod inline;
pub mod rebase;
pub mod versions;
