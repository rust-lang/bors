use regex::{Captures, Regex};

/// Replaces github @mentions with backticks to prevent accidental pings
pub fn suppress_github_mentions(text: &str) -> String {
    if !text.contains('@') {
        return text.to_string();
    }

    let pattern = r"\B(@\S+)";

    let re = Regex::new(pattern).unwrap();
    re.replace_all(text, |caps: &Captures| format!("`{}`", &caps[1]))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suppress_github_mentions() {
        assert_eq!(suppress_github_mentions("r? @matklad\n"), "r? `@matklad`\n");
        assert_eq!(suppress_github_mentions("@bors r+\n"), "`@bors` r+\n");
        assert_eq!(
            suppress_github_mentions("mail@example.com"),
            "mail@example.com"
        )
    }
}
