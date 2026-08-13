//! Terminal-output sanitization for untrusted model and status text.

/// Strip C0/C1 controls so untrusted text cannot drive the terminal device.
///
/// Newlines and tabs are preserved for wrapping. ESC, BEL, CSI/OSC C1 codes,
/// and every other control character are dropped, which guarantees that a
/// payload such as `ESC]52;...` cannot reach the TTY as a control sequence.
#[must_use]
pub fn sanitize_terminal_text(input: &str) -> String {
    input
        .chars()
        .filter(|character| matches!(character, '\n' | '\t') || !character.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_osc_clipboard_and_csi_color_sequences() {
        let sanitized = sanitize_terminal_text(
            "keep\u{1b}]52;c;SSBzdG9sZSB5b3VyIGNsaXBib2FyZA==\u{07} me\u{1b}[31m!",
        );
        assert_eq!(
            sanitized,
            "keep]52;c;SSBzdG9sZSB5b3VyIGNsaXBib2FyZA== me[31m!"
        );
        assert!(!sanitized.contains('\u{1b}'));
        assert!(!sanitized.contains('\u{07}'));
    }

    #[test]
    fn preserves_newlines_and_unicode() {
        assert_eq!(sanitize_terminal_text("λ\n世界\t☕"), "λ\n世界\t☕");
    }
}
