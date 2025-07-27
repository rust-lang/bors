//! Tree-structured comment labels.
//!
//! The tree structure mirrors the structure of how bors handles events.
//! A full path in the tree corresponds to a comment posted by bors somewhere.
//! A partial path includes its child labels, it can only be used for search
//! purposes, it do not correspond to any comment.
//!
//! It's difficult to manually maintain the tree structure,
//! so we use a macro to declare them, see `declare_labels!` below.
//! It generates the enums definitions, implements necessary traits,
//! and validates the label literals.

use std::fmt::{Display, Formatter};
use std::str::FromStr;

use anyhow::{Error, Result, anyhow};
use sqlx::postgres::types::{PgLTree, PgLTreeLabel};

/// Declare comment labels.
macro_rules! declare_labels {
    (
        $(#[$root_doc:meta])*
        pub enum $root_name:ident {
            $(
                $(#[$root_variant_doc:meta])*
                $root_variant:ident$((Option<$root_ty:ident>))? = $root_label:literal,
            )*
        }
        $(
            $(#[$enum_doc:meta])*
            pub enum $name:ident {
                $(
                    $(#[$variant_doc:meta])*
                    $variant:ident$((Option<$ty:ident>))? = $label:literal,
                )*
            }
        )*
    ) => {
        declare_private_label_trait!($root_name);
        items_for_each_label! {
            $(#[$root_doc])*
            enum $root_name {
                $(
                    $(#[$root_variant_doc])*
                    $root_variant $($root_ty)? = $root_label,
                )*
            }
        }
        $(
            items_for_each_label! {
                $(#[$enum_doc])*
                enum $name {
                    $(
                        $(#[$variant_doc])*
                        $variant $($ty)? = $label,
                    )*
                }
            }
            items_for_non_root_label! {
                $root_name
                enum $name {
                    $( $variant$($ty)?, )*
                }
            }
        )*
    };
}

/// Declare the private `Label` trait.
macro_rules! declare_private_label_trait {
    ($root_label:ident) => {
        mod private {
            use super::{PgLTree, PgLTreeLabel, Result, $root_label};

            /// Internal methods for label enums.
            ///
            /// Non-root labels only hold partial information about the tree
            /// structure. If we allow them to be converted to `PgLTree` and
            /// saved to the database, that would be a mistake.
            ///
            /// To avoid this situation, this private trait hides the
            /// implementation details and is inaccessible outside this module.
            /// Non-root labels must be first converted to root label via the
            /// `Into<$root_label>` trait bound, then use public APIs exposed
            /// by the root label enum.
            pub trait Label: Into<$root_label> {
                /// Public API: `<CommentLabel as TryFrom<PgLTree>>::try_from`
                fn from_ltree(labels: &[PgLTreeLabel]) -> Result<Self>;

                /// Public API: `<PgLTree as From<CommentLabel>>::from`
                fn to_ltree(&self, ltree: &mut PgLTree);

                /// Public API: `CommentLabel::is_partial`
                fn is_partial(&self) -> bool;
            }
        }
    };
}

/// Items for each label enum, including:
///
/// - Declare the enum.
/// - Implement `private::Label`.
/// - Implement `From<ChildLabel> for ParentLabel`.
/// - Validate label literals at compile time.
macro_rules! items_for_each_label {
    (
        $(#[$enum_doc:meta])*
        enum $name:ident {
            $(
                $(#[$variant_doc:meta])*
                $variant:ident $($ty:ident)? = $label:literal,
            )*
        }
    ) => {
        $(#[$enum_doc])*
        #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
        pub enum $name {
            $(
                $(#[$variant_doc])*
                $variant$((Option<$ty>))?,
            )*
        }

        impl private::Label for $name {
            fn from_ltree(labels: &[PgLTreeLabel]) -> Result<Self> {
                match labels {
                    $(
                        [label] if &**label == $label => Ok($name::$variant$((None::<$ty>))?),
                        $(
                            [label, rest @ ..] if &**label == $label => $ty::from_ltree(rest).map(Into::into),
                        )?
                    )*
                    _ => Err(unknown_labels(labels, join_labels!($($label)*))),
                }
            }

            #[allow(non_snake_case)]
            fn to_ltree(&self, ltree: &mut PgLTree) {
                match self {
                    $(
                        $name::$variant$(($ty))? => {
                            // Already checked that $label is a valid ltree label.
                            ltree.push(PgLTreeLabel::new($label).unwrap());
                            $(
                                if let Some(label) = $ty {
                                    label.to_ltree(ltree);
                                }
                            )?
                        }
                    )*
                }
            }

            fn is_partial(&self) -> bool {
                match self {
                    $($(
                        $name::$variant(None::<$ty>) => true,
                        $name::$variant(Some(label)) => label.is_partial(),
                    )?)*
                    #[allow(unreachable_patterns)]
                    _ => false,
                }
            }
        }

        // impl From<ChildLabel> for ParentLabel
        $($(
            impl From<$ty> for $name {
                fn from(value: $ty) -> Self {
                    $name::$variant(Some(value))
                }
            }
        )?)*

        // Check that the label literal is a valid ltree label at compile time.
        const _: () = {
            $(
                if is_invalid_ltree_label($label) {
                    panic!($label);
                }
            )*
        };
    };
}

/// Items for non-root labels. Including:
///
/// - `From<ChildLabel> for RootLabel` implementation.
macro_rules! items_for_non_root_label {
    (
        $root_name:ident
        enum $name:ident {
            $( $variant:ident $($ty:ident)?, )*
        }
    ) => {
        // impl From<ChildLabel> for RootLabel
        $($(
            impl From<$ty> for $root_name {
                fn from(value: $ty) -> Self {
                    $name::$variant(Some(value)).into()
                }
            }
        )?)*
    }
}

/// Accepts one or more label literals and joins them into an error message.
/// The message is used by `unknown_labels`.
///
/// # Example
///
/// ```ignore
/// assert_eq!(
///     join_labels!("a" "b" "c"),
///     "`a`, `b`, `c`"
/// );
/// ```
macro_rules! join_labels {
    ($first:literal $($rest:literal)*) => {
        concat!("`", $first, "`" $(, concat!(", `", $rest, "`"))*)
    };
}

// The main entry point for declaring labels.
declare_labels! {
    /// Root label for all bors comments.
    pub enum CommentLabel {
        Repository(Option<RepositoryLabel>) = "repository",
        Global(Option<GlobalLabel>) = "global",
    }

    /// Comments posted when handling repository events.
    pub enum RepositoryLabel {
        Comment(Option<PrCommentLabel>) = "comment",
    }

    /// Comments posted when handling global events.
    pub enum GlobalLabel {
        /// The configuration of some repository has been changed for the bot's Github App.
        InstallationsChanged = "installations_changed",
        /// Refresh the configuration of each tracked repository.
        RefreshConfig = "refresh_config",
        /// Refresh the team permissions.
        RefreshPermissions = "refresh_permissions",
        /// Cancel builds that have been running for a long time.
        CancelTimedOutBuilds = "cancel_timed_out_builds",
        /// Refresh mergeability status of PRs that have unknown mergeability status.
        RefreshPullRequestMergeability = "refresh_pull_request_mergeability",
        /// Periodic event that serves for synchronizing PR state.
        RefreshPullRequestState = "refresh_pull_request_state",
        /// Process the merge queue.
        ProcessMergeQueue = "process_merge_queue",
    }

    /// Comments posted when handling PR comments.
    pub enum PrCommentLabel {
        Command(Option<CommandLabel>) = "command",
    }

    /// Comments posted when handling commands.
    pub enum CommandLabel {
        Try(Option<CommandTryLabel>) = "try",
    }

    /// Comments posted when handling a `try` command.
    pub enum CommandTryLabel {
        TryBuildStarted = "try_build_started",
    }
}

// Public APIs for `CommentLabel`

impl From<CommentLabel> for PgLTree {
    fn from(label: CommentLabel) -> Self {
        let mut ltree = PgLTree::new();
        <CommentLabel as private::Label>::to_ltree(&label, &mut ltree);
        ltree
    }
}

impl TryFrom<PgLTree> for CommentLabel {
    type Error = Error;

    fn try_from(value: PgLTree) -> Result<Self, Self::Error> {
        <CommentLabel as private::Label>::from_ltree(&value)
    }
}

impl Display for CommentLabel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        <PgLTree as From<CommentLabel>>::from(*self).fmt(f)
    }
}

impl FromStr for CommentLabel {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PgLTree::from_str(s)?.try_into()
    }
}

impl CommentLabel {
    pub fn is_partial(&self) -> bool {
        <CommentLabel as private::Label>::is_partial(self)
    }
}

// Private utility functions

fn unknown_labels(labels: &[PgLTreeLabel], expected: &str) -> Error {
    if let Some(first) = labels.first() {
        anyhow!("Unknown label `{first}`. Expected one of: {expected}")
    } else {
        anyhow!("Unexpected empty labels. Expected one of: {expected}")
    }
}

/// Check if a label is a valid ltree label.
///
/// This is a const version of [`PgLTreeLabel::new`].
// FIXME: Used in `items_for_each_label!`.
// This false positive seems to be fixed in Rust 1.89
#[allow(dead_code)]
const fn is_invalid_ltree_label(label: &'static str) -> bool {
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes.len() > 256 {
        return true;
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i += 1;
        } else {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cases() -> [(CommentLabel, &'static str); 5] {
        [
            (CommentLabel::Repository(None), "repository"),
            (RepositoryLabel::Comment(None).into(), "repository.comment"),
            (
                PrCommentLabel::Command(None).into(),
                "repository.comment.command",
            ),
            (
                CommandLabel::Try(None).into(),
                "repository.comment.command.try",
            ),
            (
                CommandTryLabel::TryBuildStarted.into(),
                "repository.comment.command.try.try_build_started",
            ),
        ]
    }

    #[test]
    fn test_display() {
        for (label, expected) in test_cases() {
            assert_eq!(label.to_string(), expected);
            let ltree: PgLTree = label.into();
            assert_eq!(ltree.to_string(), expected);
        }
    }

    #[test]
    fn test_parse_success() {
        for (label, expected) in test_cases() {
            assert_eq!(expected.parse::<CommentLabel>().unwrap(), label);
        }
    }

    #[test]
    fn test_unknown_label() {
        let unkown = [
            "unknown",
            "unknown.comment",
            "repository.unknown",
            "repository.comment.unknown",
            "repository.comment.command.unknown.try_build_started",
        ];
        for label in unkown {
            let err = label.parse::<CommentLabel>().unwrap_err();
            assert!(err.to_string().starts_with("Unknown label `unknown`"));
        }
    }

    #[test]
    fn test_is_partial() {
        let partial = [
            CommentLabel::Repository(None),
            RepositoryLabel::Comment(None).into(),
            PrCommentLabel::Command(None).into(),
            CommandLabel::Try(None).into(),
        ];
        for label in partial {
            assert!(label.is_partial(), "Expected {} to be partial", label);
        }
    }

    #[test]
    fn test_is_not_partial() {
        let full: &[CommentLabel] = &[CommandTryLabel::TryBuildStarted.into()];
        for label in full {
            assert!(!label.is_partial(), "Expected {} to be full", label);
        }
    }
}
