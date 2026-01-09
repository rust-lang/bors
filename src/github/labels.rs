/// An event that may trigger some modifications of labels on a PR.
#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub enum LabelTrigger {
    /// A PR was approved with r+.
    Approved,
    /// A PR was unapproved, either with r- or it was automatically unapproved for some reason.
    Unapproved,
    /// A try build has failed.
    TryBuildFailed,
    /// An auto build triggered from the merge queue has succeeded and the PR was merged.
    AutoBuildSucceeded,
    /// An auto build triggered from the merge queue has failed.
    AutoBuildFailed,
    /// A merge conflict was detected on a pull request.
    Conflict,
}

#[derive(Debug, Eq, PartialEq)]
pub enum LabelModification {
    Add(String),
    Remove(String),
}
