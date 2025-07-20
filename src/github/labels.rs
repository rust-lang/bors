/// An event that may trigger some modifications of labels on a PR.
#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub enum LabelTrigger {
    Approved,
    Unapproved,
    TryBuildStarted,
    TryBuildSucceeded,
    TryBuildFailed,
    AutoBuildSucceeded,
    AutoBuildFailed,
}

#[derive(Debug, Eq, PartialEq)]
pub enum LabelModification {
    Add(String),
    Remove(String),
}
