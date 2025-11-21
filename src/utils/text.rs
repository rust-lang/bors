use std::borrow::Cow;

/// Pluralizes a piece of text.
pub fn pluralize(base: &str, count: usize) -> Cow<'_, str> {
    if count == 1 {
        base.into()
    } else {
        format!("{base}s").into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluralize_zero() {
        assert_eq!(pluralize("foo", 0), "foos");
    }

    #[test]
    fn pluralize_one() {
        assert_eq!(pluralize("foo", 1), "foo");
    }

    #[test]
    fn pluralize_two() {
        assert_eq!(pluralize("foo", 2), "foos");
    }
}
