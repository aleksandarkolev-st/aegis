pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .filter(
            |character| !matches!(*character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_text_preserves_unicode_and_newlines_but_removes_controls_and_bidi_overrides() {
        assert_eq!(clean("🦊 日本語\nnext"), "🦊 日本語\nnext");
        assert_eq!(clean("\0\tline\r\nend\u{7f}"), "line\nend");
        for character in ['\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}'] {
            assert_eq!(clean(&format!("left{character}right")), "leftright");
        }
        assert_eq!(clean("text\u{1b}[2J"), "text[2J");
    }
}
