use std::fmt::{self, Write};

pub(crate) struct TerminalText<'a>(pub(crate) &'a str);

impl fmt::Display for TerminalText<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for character in self.0.chars() {
            if character.is_control() && !matches!(character, '\n' | '\t') {
                write!(formatter, "{}", character.escape_default())?;
            } else {
                formatter.write_char(character)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::TerminalText;

    #[test]
    fn provider_control_sequences_are_visible_text() {
        let provider_text = "before\x1b]52;c;c3ludGhldGlj\x07after\x1b[2J\rspoof\x08\u{009b}31m\u{009d}title\u{009c}\x7f";
        let rendered = TerminalText(provider_text).to_string();
        assert!(rendered.chars().all(|character| !character.is_control()));
        assert!(rendered.contains("before"));
        assert!(rendered.contains("after"));
        assert!(rendered.contains("spoof"));
    }

    #[test]
    fn fragment_boundaries_cannot_restore_terminal_controls() {
        let provider_text = "é\x1b]52;c;c3ludGhldGlj\x1b\\\u{009b}2J終";
        let expected = TerminalText(provider_text).to_string();
        for boundary in provider_text.char_indices().map(|(index, _)| index) {
            let rendered = format!(
                "{}{}",
                TerminalText(&provider_text[..boundary]),
                TerminalText(&provider_text[boundary..])
            );
            assert_eq!(rendered, expected);
            assert!(rendered.chars().all(|character| !character.is_control()));
        }
    }

    #[test]
    fn ordinary_unicode_lines_and_tabs_remain_readable() {
        let provider_text = "Hello, 世界 👩🏽‍💻\n\tIndented line\n";
        assert_eq!(TerminalText(provider_text).to_string(), provider_text);
    }
}
