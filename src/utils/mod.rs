pub mod logging;
pub mod timing;

/// Converts GitHub @mentions to markdown-backticked text to prevent notifications.
/// For example, "@user" becomes "`user`".
///
/// Handles GitHub mention formats:
/// - Usernames (@username)
/// - Teams (@org/team)
/// - Nested teams (@org/team/subteam)
///
/// GitHub's nested team documentation:
/// https://docs.github.com/en/organizations/organizing-members-into-teams/about-teams#nested-teams
///
/// Ignores email addresses and other @ symbols that don't match GitHub mention patterns.
pub fn suppress_github_mentions(text: &str) -> String {
    if text.is_empty() || !text.contains('@') {
        return text.to_string();
    }

    let segment = r"[A-Za-z0-9][A-Za-z0-9\-]{0,38}";
    let pattern = format!(r"@{0}(?:/{0})*", segment);

    let re = regex::Regex::new(&pattern).unwrap();
    re.replace_all(text, |caps: &regex::Captures| {
        let mention = &caps[0];
        let position = caps.get(0).unwrap().start();

        if !is_github_mention(text, mention, position) {
            return mention.to_string();
        }

        let name = &mention[1..]; // Drop the @ symbol
        format!("`{}`", name)
    })
    .to_string()
}

// Determines if a potential mention would actually trigger a notification
fn is_github_mention(text: &str, mention: &str, pos: usize) -> bool {
    // Not a valid mention if preceded by alphanumeric or underscore (email)
    if pos > 0 {
        let c = text.chars().nth(pos - 1).unwrap();
        if c.is_alphanumeric() || c == '_' {
            return false;
        }
    }

    // Check if followed by invalid character
    let end = pos + mention.len();
    if end < text.len() {
        let next_char = text.chars().nth(end).unwrap();
        if next_char.is_alphanumeric() || next_char == '_' || next_char == '-' {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suppress_github_mentions() {
        // User mentions
        assert_eq!(suppress_github_mentions("Hello @user"), "Hello `user`");

        // Org team mentions
        assert_eq!(suppress_github_mentions("@org/team"), "`org/team`");
        assert_eq!(
            suppress_github_mentions("@org/team/subteam"),
            "`org/team/subteam`"
        );
        assert_eq!(
            suppress_github_mentions("@big/team/sub/group"),
            "`big/team/sub/group`"
        );
        assert_eq!(
            suppress_github_mentions("Thanks @user, @rust-lang/libs and @github/docs/content!"),
            "Thanks `user`, `rust-lang/libs` and `github/docs/content`!"
        );

        // Non mentions
        assert_eq!(suppress_github_mentions("@"), "@");
        assert_eq!(suppress_github_mentions(""), "");
        assert_eq!(
            suppress_github_mentions("No mentions here"),
            "No mentions here"
        );
        assert_eq!(
            suppress_github_mentions("user@example.com"),
            "user@example.com"
        );

        assert_eq!(suppress_github_mentions("@user_test"), "@user_test");
    }
}
