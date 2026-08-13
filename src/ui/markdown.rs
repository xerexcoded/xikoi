//! Small, terminal-oriented Markdown formatter.
//!
//! This intentionally produces owned Ratatui lines.  The transcript can then
//! be wrapped into deterministic terminal rows before applying its scroll
//! offset, without opting into Ratatui's unstable rendered-line measurement.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::domain::{ChatMessage, MessageOrigin, MessageStatus, Role};

use super::{Theme, sanitize_terminal_text};

#[derive(Debug)]
struct MarkdownWriter {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    prefix: Option<Span<'static>>,
    theme: Theme,
    code_block_depth: usize,
}

impl MarkdownWriter {
    fn new(theme: Theme) -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            style_stack: vec![theme.text],
            prefix: None,
            theme,
            code_block_depth: 0,
        }
    }

    fn style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or(self.theme.text)
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(self.style().patch(style));
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn ensure_prefix(&mut self) {
        if self.spans.is_empty()
            && let Some(prefix) = self.prefix.clone()
        {
            self.spans.push(prefix);
        }
    }

    fn push_text(&mut self, text: &str, style: Style) {
        let mut remaining = text;
        while let Some((line, tail)) = remaining.split_once('\n') {
            if !line.is_empty() {
                self.ensure_prefix();
                self.spans.push(Span::styled(line.to_owned(), style));
            }
            self.flush_line();
            remaining = tail;
        }
        if !remaining.is_empty() {
            self.ensure_prefix();
            self.spans.push(Span::styled(remaining.to_owned(), style));
        }
    }

    fn flush_line(&mut self) {
        if self.spans.is_empty() {
            self.lines.push(Line::default());
        } else {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }

    fn flush_if_nonempty(&mut self) {
        if !self.spans.is_empty() {
            self.flush_line();
        }
    }

    fn blank_line(&mut self) {
        self.flush_if_nonempty();
        if self.lines.last().is_none_or(|line| !line.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_if_nonempty();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

/// Convert Markdown into styled, owned terminal lines.
///
/// Paragraphs, headings, lists, quotes, emphasis, links, inline code, and
/// fenced code blocks receive distinct terminal-safe styling.  Unknown
/// extensions degrade to their text content.
#[allow(clippy::match_same_arms, clippy::too_many_lines)]
pub fn markdown_lines(markdown: &str, theme: Theme) -> Vec<Line<'static>> {
    let markdown = sanitize_terminal_text(markdown);
    let mut writer = MarkdownWriter::new(theme);
    let parser = Parser::new_ext(
        &markdown,
        Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_TABLES
            | Options::ENABLE_FOOTNOTES,
    );

    for event in parser {
        match event {
            Event::Start(Tag::Paragraph) => writer.flush_if_nonempty(),
            Event::End(TagEnd::Paragraph) => writer.blank_line(),
            Event::Start(Tag::Heading { .. }) => {
                writer.flush_if_nonempty();
                writer.push_style(theme.accent.add_modifier(Modifier::BOLD));
            }
            Event::End(TagEnd::Heading(_)) => {
                writer.flush_if_nonempty();
                writer.pop_style();
                writer.blank_line();
            }
            Event::Start(Tag::CodeBlock(_)) => {
                writer.flush_if_nonempty();
                writer.code_block_depth = writer.code_block_depth.saturating_add(1);
                writer.prefix = Some(Span::styled(theme.quote().to_owned(), theme.code_block));
                writer.push_style(theme.code_block);
            }
            Event::End(TagEnd::CodeBlock) => {
                writer.flush_if_nonempty();
                writer.code_block_depth = writer.code_block_depth.saturating_sub(1);
                writer.prefix = None;
                writer.pop_style();
                writer.blank_line();
            }
            Event::Start(Tag::Emphasis) => {
                writer.push_style(Style::new().add_modifier(Modifier::ITALIC));
            }
            Event::End(TagEnd::Emphasis) => writer.pop_style(),
            Event::Start(Tag::Strong) => {
                writer.push_style(Style::new().add_modifier(Modifier::BOLD));
            }
            Event::End(TagEnd::Strong) => writer.pop_style(),
            Event::Start(Tag::Strikethrough) => {
                writer.push_style(Style::new().add_modifier(Modifier::CROSSED_OUT));
            }
            Event::End(TagEnd::Strikethrough) => writer.pop_style(),
            Event::Start(Tag::Link { .. }) => {
                writer.push_style(theme.secondary.add_modifier(Modifier::UNDERLINED));
            }
            Event::End(TagEnd::Link) => writer.pop_style(),
            Event::Start(Tag::BlockQuote(_)) => {
                writer.flush_if_nonempty();
                writer.prefix = Some(Span::styled(theme.quote().to_owned(), theme.muted));
                writer.push_style(theme.muted);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                writer.flush_if_nonempty();
                writer.prefix = None;
                writer.pop_style();
                writer.blank_line();
            }
            Event::Start(Tag::Item) => {
                writer.flush_if_nonempty();
                writer.prefix = Some(Span::styled(theme.bullet().to_owned(), theme.accent));
            }
            Event::End(TagEnd::Item) => {
                writer.flush_if_nonempty();
                writer.prefix = None;
            }
            Event::End(TagEnd::List(_)) => writer.blank_line(),
            Event::Start(Tag::TableCell) => {
                if !writer.spans.is_empty() {
                    writer.spans.push(Span::styled(
                        if theme.ascii { " | " } else { " │ " }.to_owned(),
                        theme.muted,
                    ));
                }
            }
            Event::End(TagEnd::TableRow) => writer.flush_if_nonempty(),
            Event::Text(text) => {
                let style = if writer.code_block_depth > 0 {
                    theme.code_block
                } else {
                    writer.style()
                };
                writer.push_text(&text, style);
            }
            Event::Code(code) => {
                writer.ensure_prefix();
                writer
                    .spans
                    .push(Span::styled(format!(" {code} "), theme.code));
            }
            Event::SoftBreak => writer.push_text(" ", writer.style()),
            Event::HardBreak => writer.flush_line(),
            Event::Rule => {
                writer.flush_if_nonempty();
                writer
                    .lines
                    .push(Line::styled(theme.horizontal_rule(), theme.muted));
            }
            Event::TaskListMarker(checked) => {
                let marker = match (theme.ascii, checked) {
                    (true, true) => "[x] ",
                    (true, false) => "[ ] ",
                    (false, true) => "☑ ",
                    (false, false) => "☐ ",
                };
                writer.ensure_prefix();
                writer
                    .spans
                    .push(Span::styled(marker.to_owned(), theme.accent));
            }
            Event::InlineHtml(html) | Event::Html(html) => {
                writer.push_text(&html, writer.style());
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                writer.push_text(&math, theme.code);
            }
            Event::FootnoteReference(reference) => {
                writer.push_text(&format!("[{reference}]"), theme.secondary);
            }
            Event::Start(_) | Event::End(_) => {}
        }
    }

    writer.finish()
}

/// Render one domain message, including a semantic role/status header.
pub fn message_lines(message: &ChatMessage, theme: Theme) -> Vec<Line<'static>> {
    let role_style = theme.role_style(message.role);
    let role = match message.role {
        Role::System => "SYSTEM",
        Role::User => "USER",
        Role::Assistant => "ASSISTANT",
    };
    let mut header = vec![
        Span::styled(theme.role_glyph(message.role).to_owned(), role_style),
        Span::raw(" "),
        Span::styled(role.to_owned(), role_style.add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" // {}", message.created_at.format("%H:%M:%S")),
            theme.muted,
        ),
    ];

    if message.origin == MessageOrigin::Speech {
        header.push(Span::styled("  VOICE".to_owned(), theme.secondary));
    }
    if let Some(model) = message.model.as_deref() {
        header.push(Span::styled(
            format!("  {}", sanitize_terminal_text(model)),
            theme.muted,
        ));
    }
    match message.status {
        MessageStatus::Complete => {}
        MessageStatus::Streaming => {
            header.push(Span::styled("  STREAMING".to_owned(), theme.secondary));
        }
        MessageStatus::Cancelled => {
            header.push(Span::styled("  CANCELLED".to_owned(), theme.warning));
        }
        MessageStatus::Failed => {
            header.push(Span::styled("  FAILED".to_owned(), theme.danger));
        }
    }

    let mut lines = vec![Line::from(header)];
    if message.content.is_empty() && message.status == MessageStatus::Streaming {
        lines.push(Line::styled(
            if theme.ascii {
                "... receiving"
            } else {
                "… receiving"
            },
            theme.muted,
        ));
    } else {
        lines.extend(markdown_lines(&message.content, theme));
    }
    if let Some(error) = message.error.as_deref() {
        lines.push(Line::styled(
            format!("ERROR // {}", sanitize_terminal_text(error)),
            theme.danger,
        ));
    }
    lines.push(Line::default());
    lines
}

/// Wrap styled lines by terminal-cell width while preserving span styles and
/// Unicode grapheme clusters.
///
/// Wrapping is character-based by design.  It is deterministic and gives the
/// transcript an exact row count for scrolling.
pub fn wrap_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let mut output = Vec::new();
    for line in lines {
        if line.spans.is_empty() {
            output.push(Line::default());
            continue;
        }

        let mut current_spans = Vec::new();
        let mut current_width = 0usize;

        for span in line.spans {
            let style = span.style;
            let mut segment = String::new();

            for grapheme in span.content.graphemes(true) {
                let grapheme_width = UnicodeWidthStr::width(grapheme);
                if grapheme_width > 0
                    && current_width > 0
                    && current_width.saturating_add(grapheme_width) > width
                {
                    if !segment.is_empty() {
                        current_spans.push(Span::styled(std::mem::take(&mut segment), style));
                    }
                    output.push(Line::from(std::mem::take(&mut current_spans)));
                    current_width = 0;
                }

                segment.push_str(grapheme);
                current_width = current_width.saturating_add(grapheme_width);

                if current_width >= width {
                    current_spans.push(Span::styled(std::mem::take(&mut segment), style));
                    output.push(Line::from(std::mem::take(&mut current_spans)));
                    current_width = 0;
                }
            }

            if !segment.is_empty() {
                current_spans.push(Span::styled(segment, style));
            }
        }

        if !current_spans.is_empty() {
            output.push(Line::from(current_spans));
        } else if current_width == 0
            && output
                .last()
                .is_none_or(|last: &Line<'_>| !last.spans.is_empty())
        {
            output.push(Line::default());
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::{config::ColorMode, domain::MessageStatus, ui::ThemeOptions};

    fn theme() -> Theme {
        Theme::new(ThemeOptions::new(ColorMode::Ansi))
    }

    fn ascii_theme() -> Theme {
        Theme::new(ThemeOptions::new(ColorMode::Mono).with_ascii(true))
    }

    #[test]
    fn preserves_unicode_and_styles_inline_and_fenced_code() {
        let lines = markdown_lines(
            "Hello **世界** and `λx`.\n\n```rust\nlet café = \"☕\";\n```",
            theme(),
        );
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("世界"));
        assert!(rendered.contains("λx"));
        assert!(rendered.contains("café"));
        assert!(rendered.contains('☕'));
        assert!(
            lines
                .iter()
                .flatten()
                .any(|span| span.style == theme().code)
        );
        assert!(
            lines
                .iter()
                .flatten()
                .any(|span| span.style == theme().code_block)
        );
    }

    #[test]
    fn wraps_without_splitting_wide_graphemes() {
        let lines = vec![Line::from(Span::raw("ab界cd"))];
        let wrapped = wrap_lines(lines, 4);
        let content = wrapped.iter().map(Line::to_string).collect::<Vec<_>>();
        assert_eq!(content, ["ab界", "cd"]);
    }

    #[test]
    fn message_header_exposes_role_and_streaming_state() {
        let message = ChatMessage {
            id: Uuid::now_v7(),
            created_at: Utc::now(),
            role: Role::Assistant,
            content: String::new(),
            origin: MessageOrigin::Generated,
            status: MessageStatus::Streaming,
            model: Some("qwen3.5:9b".into()),
            finish_reason: None,
            usage: None,
            error: None,
        };
        let text = message_lines(&message, theme())
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("ASSISTANT"));
        assert!(text.contains("STREAMING"));
        assert!(text.contains("qwen3.5:9b"));
    }

    #[test]
    fn cancelled_and_failed_messages_have_explicit_terminal_states() {
        let base = ChatMessage {
            id: Uuid::now_v7(),
            created_at: Utc::now(),
            role: Role::Assistant,
            content: "partial response".to_owned(),
            origin: MessageOrigin::Generated,
            status: MessageStatus::Cancelled,
            model: Some("qwen3.5:9b".into()),
            finish_reason: Some("cancelled".to_owned()),
            usage: None,
            error: None,
        };
        let cancelled = message_lines(&base, theme())
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(cancelled.contains("CANCELLED"));
        assert!(cancelled.contains("partial response"));

        let mut failed_message = base;
        failed_message.status = MessageStatus::Failed;
        failed_message.error = Some("model runner stopped".to_owned());
        let failed = message_lines(&failed_message, theme())
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(failed.contains("FAILED"));
        assert!(failed.contains("ERROR // model runner stopped"));
    }

    #[test]
    fn ascii_markdown_uses_plain_table_and_code_prefixes() {
        let lines = markdown_lines(
            "| key | value |\n| --- | --- |\n| λ | 世界 |\n\n```\nlocal()\n```",
            ascii_theme(),
        );
        let rendered = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(" | "));
        assert!(rendered.contains("| local()"));
        assert!(!rendered.contains('│'));
    }

    #[test]
    fn control_sequences_cannot_reach_rendered_spans() {
        let payload =
            "safe\u{1b}]52;c;SSBzdG9sZSB5b3VyIGNsaXBib2FyZA==\u{07} text \u{1b}[31mred\u{1b}[0m";
        let message = ChatMessage {
            id: Uuid::now_v7(),
            created_at: Utc::now(),
            role: Role::Assistant,
            content: payload.to_owned(),
            origin: MessageOrigin::Generated,
            status: MessageStatus::Failed,
            model: Some("qwen\u{1b}]0;hijack\u{07}".into()),
            finish_reason: None,
            usage: None,
            error: Some("boom\u{1b}[2J".to_owned()),
        };
        let rendered = message_lines(&message, theme())
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("safe"));
        assert!(rendered.contains("text"));
        assert!(rendered.contains("red"));
        assert!(rendered.contains("ERROR // boom"));
        assert!(
            !rendered.contains('\u{1b}'),
            "ESC must not reach the terminal: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{07}'),
            "BEL must not reach the terminal: {rendered:?}"
        );
        assert!(
            !rendered.chars().any(char::is_control),
            "control bytes leaked into spans: {rendered:?}"
        );
    }
}
