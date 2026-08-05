//! Escaping for every string the app did not write itself.

use std::fmt::{self, Display, Formatter, Write as _};

/// A borrowed string that renders as HTML text and never as markup.
pub struct Escaped<'a>(&'a str);

/// Wrap `value` so that a template placeholder cannot smuggle markup through.
#[must_use]
pub const fn text(value: &str) -> Escaped<'_> {
    Escaped(value)
}

impl Display for Escaped<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        for character in self.0.chars() {
            match character {
                '&' => formatter.write_str("&amp;")?,
                '<' => formatter.write_str("&lt;")?,
                '>' => formatter.write_str("&gt;")?,
                '"' => formatter.write_str("&quot;")?,
                '\'' => formatter.write_str("&#39;")?,
                _ => formatter.write_char(character)?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through_unchanged() {
        assert_eq!(
            text("chat/2026-08-05-slug").to_string(),
            "chat/2026-08-05-slug"
        );
    }

    #[test]
    fn a_tag_cannot_survive_into_the_document() {
        assert_eq!(
            text("<script>alert(1)</script>").to_string(),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
    }

    #[test]
    fn quotes_cannot_break_out_of_an_attribute() {
        assert_eq!(
            text("\" onclick='x'").to_string(),
            "&quot; onclick=&#39;x&#39;"
        );
    }

    #[test]
    fn an_ampersand_is_escaped_before_anything_that_follows_it() {
        assert_eq!(text("&lt;").to_string(), "&amp;lt;");
    }
}
