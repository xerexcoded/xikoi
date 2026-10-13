use std::{cmp, fmt::Write};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Rect, Size},
    symbols,
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, LineGauge, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Sparkline,
    },
};
use url::Url;

use crate::{audio::AudioDirection, domain::ChatMessage};

use super::{
    ActivityState, AudioChoice, HelpBinding, InteractionMode, ModelChoice, OverlayView,
    RouteLocation, SessionChoice, TelemetryView, Theme, ThemeOptions, TranscriptAnchor,
    TranscriptScroll, UiView, VoiceState, markdown::message_lines, render_scope,
    sanitize_terminal_text, wrap_lines,
};

const WIDE_MIN_WIDTH: u16 = 110;
const WIDE_MIN_HEIGHT: u16 = 32;
const COMPACT_MIN_WIDTH: u16 = 80;
const COMPACT_MIN_HEIGHT: u16 = 24;
const MINIMAL_MIN_WIDTH: u16 = 60;
const MINIMAL_MIN_HEIGHT: u16 = 18;
const CONTEXT_WARNING_RATIO: f64 = 0.8;

/// The exact responsive mode selected for a terminal size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutClass {
    Wide,
    Compact,
    Minimal,
    TooSmall,
}

/// Classify terminal dimensions using Xikoi's fixed, testable breakpoints.
pub const fn layout_class(size: Size) -> LayoutClass {
    if size.width >= WIDE_MIN_WIDTH && size.height >= WIDE_MIN_HEIGHT {
        LayoutClass::Wide
    } else if size.width >= COMPACT_MIN_WIDTH && size.height >= COMPACT_MIN_HEIGHT {
        LayoutClass::Compact
    } else if size.width >= MINIMAL_MIN_WIDTH && size.height >= MINIMAL_MIN_HEIGHT {
        LayoutClass::Minimal
    } else {
        LayoutClass::TooSmall
    }
}

/// Render one complete frame from borrowed application state.
pub fn render(frame: &mut Frame<'_>, view: &UiView<'_, '_>) {
    let area = frame.area();
    let theme = Theme::new(view.theme);
    frame.render_widget(Block::new().style(theme.base), area);

    let class = layout_class(Size::new(area.width, area.height));
    match class {
        LayoutClass::Wide => render_wide(frame, area, view, theme),
        LayoutClass::Compact => render_compact(frame, area, view, theme),
        LayoutClass::Minimal => render_minimal(frame, area, view, theme),
        LayoutClass::TooSmall => {
            render_too_small(frame, area, theme);
            return;
        }
    }

    if let Some(overlay) = view.overlay {
        render_overlay(frame, area, overlay, &view.telemetry, view.activity, theme);
    }
}

fn render_wide(frame: &mut Frame<'_>, area: Rect, view: &UiView<'_, '_>, theme: Theme) {
    let canvas = inset_horizontally(area, 1);
    let [activity, transcript, lower, footer] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Fill(1),
        Constraint::Length(10),
        Constraint::Length(1),
    ])
    .areas(canvas);
    let [input, telemetry] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(36)])
        .spacing(1)
        .areas(lower);

    render_wide_activity_strip(frame, activity, view, theme);
    render_transcript(
        frame,
        transcript,
        view.messages,
        view.transcript_scroll,
        view.telemetry.chat_route,
        view.telemetry.speech_route,
        theme,
    );
    render_input(frame, input, view, theme, true);
    render_telemetry_panel(frame, telemetry, &view.telemetry, view.activity, theme);
    render_footer(frame, footer, view, theme, LayoutClass::Wide);
}

fn render_compact(frame: &mut Frame<'_>, area: Rect, view: &UiView<'_, '_>, theme: Theme) {
    let [activity, transcript, input, status] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(6),
        Constraint::Length(1),
    ])
    .areas(area);

    render_activity_strip(frame, activity, view, theme, 44);
    render_transcript(
        frame,
        transcript,
        view.messages,
        view.transcript_scroll,
        view.telemetry.chat_route,
        view.telemetry.speech_route,
        theme,
    );
    render_input(frame, input, view, theme, true);
    render_footer(frame, status, view, theme, LayoutClass::Compact);
}

fn render_minimal(frame: &mut Frame<'_>, area: Rect, view: &UiView<'_, '_>, theme: Theme) {
    let [activity, transcript, input, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .areas(area);

    render_minimal_activity(frame, activity, view, theme);
    render_transcript(
        frame,
        transcript,
        view.messages,
        view.transcript_scroll,
        view.telemetry.chat_route,
        view.telemetry.speech_route,
        theme,
    );
    render_input(frame, input, view, theme, false);
    render_footer(frame, status, view, theme, LayoutClass::Minimal);
}

fn render_wide_activity_strip(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &UiView<'_, '_>,
    theme: Theme,
) {
    if area.is_empty() {
        return;
    }
    let title = if view.voice.mode == InteractionMode::Voice {
        " XIKOI // VOICE SCOPE // AI "
    } else {
        " XIKOI // LINK ACTIVITY // NEURAL SIGNAL "
    };
    let block = panel_block(title, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let [status, signal] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(activity_status_spans(view, theme, true))).style(theme.panel),
        status,
    );
    if view.voice.mode == InteractionMode::Voice {
        render_ai_scope(frame, signal, view, theme);
    } else {
        render_link_signal(frame, signal, view, theme);
    }
}

fn render_minimal_activity(frame: &mut Frame<'_>, area: Rect, view: &UiView<'_, '_>, theme: Theme) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(activity_status_spans(view, theme, true))).style(theme.panel),
        area,
    );
}

fn render_activity_strip(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &UiView<'_, '_>,
    theme: Theme,
    status_width: u16,
) {
    if area.is_empty() {
        return;
    }
    let block = panel_block(" XIKOI // LINK ACTIVITY ", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let status_width = status_width.min(inner.width);
    let [status, signal] =
        Layout::horizontal([Constraint::Length(status_width), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(activity_status_spans(view, theme, false)))
            .style(theme.panel)
            .alignment(Alignment::Left),
        status,
    );

    if signal.width == 0 {
        return;
    }
    if let Some(message) = view.status_message {
        frame.render_widget(
            Paragraph::new(sanitize_terminal_text(message))
                .style(if matches!(view.activity, super::ActivityState::Failed) {
                    theme.danger
                } else {
                    theme.muted
                })
                .alignment(Alignment::Right),
            signal,
        );
        return;
    }
    render_link_signal(frame, signal, view, theme);
}

fn activity_status_spans(
    view: &UiView<'_, '_>,
    theme: Theme,
    include_message: bool,
) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(
            format!("{} LINK {}", theme.link_glyph(view.link), view.link.label()),
            theme.link_style(view.link),
        ),
        Span::styled("  //  ".to_owned(), theme.muted),
        Span::styled(
            format!(
                "{} ACTIVITY {}",
                theme.activity_glyph(view.activity),
                view.activity.label()
            ),
            theme.activity_style(view.activity),
        ),
    ];
    if view.voice.mode == InteractionMode::Voice {
        spans.push(Span::styled("  //  ".to_owned(), theme.muted));
        spans.push(Span::styled(
            format!("VOICE {}", view.voice.state.label()),
            theme.activity_style(view.activity),
        ));
    }
    if include_message && let Some(message) = view.status_message {
        spans.push(Span::styled("  //  ".to_owned(), theme.muted));
        spans.push(Span::styled(
            sanitize_terminal_text(message),
            if matches!(view.activity, ActivityState::Failed) {
                theme.danger
            } else {
                theme.muted
            },
        ));
    }
    spans
}

fn render_link_signal(frame: &mut Frame<'_>, area: Rect, view: &UiView<'_, '_>, theme: Theme) {
    if area.is_empty() {
        return;
    }
    if theme.ascii {
        let waveform = ascii_signal(view.link_signal_samples, usize::from(area.width));
        frame.render_widget(
            Paragraph::new(waveform)
                .style(theme.activity_style(view.activity))
                .alignment(Alignment::Right),
            area,
        );
    } else {
        let baseline = [0];
        let data = if view.link_signal_samples.is_empty() {
            baseline.as_slice()
        } else {
            view.link_signal_samples
        };
        let maximum = data.iter().copied().max().unwrap_or(1).max(1);
        let sparkline = Sparkline::default()
            .data(data)
            .max(maximum)
            .bar_set(symbols::bar::NINE_LEVELS)
            .style(theme.activity_style(view.activity));
        frame.render_widget(sparkline, area);
    }
}

fn render_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    messages: &[ChatMessage],
    scroll: TranscriptScroll,
    chat_route: RouteLocation,
    speech_route: RouteLocation,
    theme: Theme,
) {
    if area.is_empty() {
        return;
    }
    let title = Line::from(vec![
        Span::styled(" TRANSCRIPT // CHAT:", theme.muted),
        Span::styled(chat_route.label(), route_style(chat_route, theme)),
        Span::styled("  VOICE:", theme.muted),
        Span::styled(speech_route.label(), route_style(speech_route, theme)),
        Span::raw(" "),
    ]);
    let block = panel_block(title, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let show_scrollbar_column = inner.width > 1;
    let content_width = if show_scrollbar_column {
        inner.width.saturating_sub(1)
    } else {
        inner.width
    };
    let content_area = Rect {
        width: content_width,
        ..inner
    };
    if content_area.is_empty() {
        return;
    }

    let (wrapped, anchors) = wrapped_transcript(messages, content_area.width, theme);
    let content_rows = wrapped.len();
    let viewport_rows = usize::from(content_area.height);
    let offset = resolve_transcript_scroll(scroll, &anchors, viewport_rows);
    let visible = wrapped
        .into_iter()
        .skip(offset)
        .take(viewport_rows)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible).style(theme.panel), content_area);

    if show_scrollbar_column && content_rows > viewport_rows && area.height > 2 {
        let mut state = ScrollbarState::new(content_rows)
            .position(offset)
            .viewport_content_length(viewport_rows);
        let scrollbar = if theme.ascii {
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("|"))
                .thumb_symbol("#")
        } else {
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("█")
        }
        .track_style(theme.scrollbar_track)
        .thumb_style(theme.scrollbar_thumb);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

/// Move the transcript by one page while retaining the identity of its top
/// message row as the transcript grows or the terminal reflows.
pub fn page_transcript(
    messages: &[ChatMessage],
    scroll: TranscriptScroll,
    screen: Size,
    options: ThemeOptions,
    up: bool,
    page_rows: usize,
) -> TranscriptScroll {
    let Some((content_width, viewport_rows)) = transcript_content_size(screen) else {
        return scroll;
    };
    let theme = Theme::new(options);
    let (_, anchors) = wrapped_transcript(messages, content_width, theme);
    let content_rows = anchors.len();
    let viewport_rows = usize::from(viewport_rows);
    let maximum = content_rows.saturating_sub(viewport_rows);
    let current = resolve_transcript_scroll(scroll, &anchors, viewport_rows);
    let target = if up {
        current.saturating_sub(page_rows)
    } else {
        current.saturating_add(page_rows).min(maximum)
    };
    if target >= maximum {
        return TranscriptScroll::following_tail();
    }
    TranscriptScroll {
        offset_from_tail: maximum.saturating_sub(target),
        follow_tail: false,
        anchor: anchors.get(target).copied().flatten(),
    }
}

fn wrapped_transcript(
    messages: &[ChatMessage],
    width: u16,
    theme: Theme,
) -> (Vec<Line<'static>>, Vec<Option<TranscriptAnchor>>) {
    if messages.is_empty() {
        let lines = wrap_lines(empty_transcript(theme), width);
        let anchors = vec![None; lines.len()];
        return (lines, anchors);
    }

    let mut wrapped = Vec::new();
    let mut anchors = Vec::new();
    for message in messages {
        let message_lines = wrap_lines(message_lines(message, theme), width);
        anchors.extend((0..message_lines.len()).map(|row_in_message| {
            Some(TranscriptAnchor {
                message_id: message.id,
                row_in_message,
            })
        }));
        wrapped.extend(message_lines);
    }
    (wrapped, anchors)
}

fn resolve_transcript_scroll(
    scroll: TranscriptScroll,
    anchors: &[Option<TranscriptAnchor>],
    viewport_rows: usize,
) -> usize {
    if scroll.follow_tail {
        return anchors.len().saturating_sub(viewport_rows);
    }
    if let Some(anchor) = scroll.anchor
        && let Some(position) = anchors
            .iter()
            .position(|row| row.is_some_and(|row| row == anchor))
    {
        return position;
    }
    if let Some(anchor) = scroll.anchor
        && let Some((position, _)) = anchors
            .iter()
            .enumerate()
            .filter_map(|(position, row)| {
                row.as_ref()
                    .filter(|row| row.message_id == anchor.message_id)
                    .map(|row| (position, *row))
            })
            .min_by_key(|(_, row)| row.row_in_message.abs_diff(anchor.row_in_message))
    {
        return position;
    }
    anchors
        .len()
        .saturating_sub(viewport_rows)
        .saturating_sub(scroll.offset_from_tail)
}

fn transcript_content_size(screen: Size) -> Option<(u16, u16)> {
    let area = Rect::new(0, 0, screen.width, screen.height);
    let transcript = match layout_class(screen) {
        LayoutClass::Wide => {
            let canvas = inset_horizontally(area, 1);
            let [_, transcript, _, _] = Layout::vertical([
                Constraint::Length(8),
                Constraint::Fill(1),
                Constraint::Length(10),
                Constraint::Length(1),
            ])
            .areas(canvas);
            transcript
        }
        LayoutClass::Compact => {
            let [_, transcript, _, _] = Layout::vertical([
                Constraint::Length(3),
                Constraint::Fill(1),
                Constraint::Length(6),
                Constraint::Length(1),
            ])
            .areas(area);
            transcript
        }
        LayoutClass::Minimal => {
            let [_, transcript, _, _] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(4),
                Constraint::Length(1),
            ])
            .areas(area);
            transcript
        }
        LayoutClass::TooSmall => return None,
    };
    let inner_width = transcript.width.saturating_sub(2);
    let content_width = if inner_width > 1 {
        inner_width.saturating_sub(1)
    } else {
        inner_width
    };
    Some((content_width, transcript.height.saturating_sub(2)))
}

fn render_input(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &UiView<'_, '_>,
    theme: Theme,
    show_signal: bool,
) {
    if area.is_empty() {
        return;
    }
    let title = format!(" {} ", view.input_title);
    let block = panel_block(title, theme).border_style(theme.secondary);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    if view.voice.mode == InteractionMode::Voice {
        render_voice_deck(frame, inner, view, theme);
        return;
    }

    if !show_signal || view.input_signal_samples.is_empty() || inner.height < 2 {
        frame.render_widget(view.input, inner);
    } else {
        let [composer, input_signal] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
        frame.render_widget(view.input, composer);
        render_input_signal(frame, input_signal, view.input_signal_samples, theme);
    }
}

fn render_voice_deck(frame: &mut Frame<'_>, area: Rect, view: &UiView<'_, '_>, theme: Theme) {
    let state_style = if view.voice.state == VoiceState::Failed {
        theme.danger
    } else if matches!(
        view.voice.state,
        VoiceState::Listening | VoiceState::Speaking
    ) {
        theme.accent
    } else {
        theme.secondary
    };
    let server = if view.voice.speech_online {
        "MLX ONLINE"
    } else {
        "MLX OFFLINE"
    };
    let header = Line::from(vec![
        Span::styled(" NEURAL VOICE // ", theme.muted),
        Span::styled(view.voice.state.label().to_owned(), state_style),
        Span::styled(format!(" // {server}"), theme.muted),
    ]);

    if area.height <= 1 {
        frame.render_widget(Paragraph::new(header).style(theme.panel), area);
        return;
    }

    if area.height < 4 {
        let [header_area, waves] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
        frame.render_widget(Paragraph::new(header).style(theme.panel), header_area);
        render_user_scope(frame, waves, view, theme);
        return;
    }

    let show_devices = area.height >= 6;
    if show_devices {
        let [header_area, scopes, devices] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(header).style(theme.panel), header_area);
        render_user_scope(frame, scopes, view, theme);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" MIC ", theme.muted),
                Span::styled(view.voice.input_device.to_owned(), theme.text),
                Span::styled("  //  OUT ", theme.muted),
                Span::styled(view.voice.output_device.to_owned(), theme.text),
            ]))
            .style(theme.panel),
            devices,
        );
    } else {
        let [header_area, scopes] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
        frame.render_widget(Paragraph::new(header).style(theme.panel), header_area);
        render_user_scope(frame, scopes, view, theme);
    }
}

fn render_user_scope(frame: &mut Frame<'_>, area: Rect, view: &UiView<'_, '_>, theme: Theme) {
    let user_style = if view.voice.state == VoiceState::Listening {
        theme.accent
    } else if view.voice.state == VoiceState::Failed {
        theme.danger
    } else {
        theme.secondary
    };
    render_scope(
        frame,
        area,
        "USER",
        view.user_scope_samples,
        user_style,
        theme,
    );
}

fn render_ai_scope(frame: &mut Frame<'_>, area: Rect, view: &UiView<'_, '_>, theme: Theme) {
    let ai_style = if matches!(
        view.voice.state,
        VoiceState::Speaking | VoiceState::Synthesizing
    ) {
        theme.accent
    } else if view.voice.state == VoiceState::Failed {
        theme.danger
    } else {
        theme.secondary
    };
    render_scope(frame, area, "AI", view.ai_scope_samples, ai_style, theme);
}

fn render_input_signal(frame: &mut Frame<'_>, area: Rect, samples: &[u64], theme: Theme) {
    if area.is_empty() {
        return;
    }
    let label_width = 13.min(area.width);
    let [label, signal] =
        Layout::horizontal([Constraint::Length(label_width), Constraint::Fill(1)]).areas(area);
    frame.render_widget(
        Paragraph::new(if theme.ascii {
            " KEY ECHO // "
        } else {
            " KEY ECHO ∿ "
        })
        .style(theme.muted),
        label,
    );
    if signal.is_empty() {
        return;
    }
    if theme.ascii {
        frame.render_widget(
            Paragraph::new(ascii_signal(samples, usize::from(signal.width))).style(theme.secondary),
            signal,
        );
    } else {
        let maximum = samples.iter().copied().max().unwrap_or(1).max(1);
        frame.render_widget(
            Sparkline::default()
                .data(samples)
                .max(maximum)
                .bar_set(symbols::bar::THREE_LEVELS)
                .style(theme.secondary),
            signal,
        );
    }
}

fn render_telemetry_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    telemetry: &TelemetryView<'_>,
    activity: ActivityState,
    theme: Theme,
) {
    if area.is_empty() {
        return;
    }
    let block = panel_block(" SYSTEM TELEMETRY ", theme).border_style(theme.accent);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let [details, gauge] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    let lines = telemetry_summary_lines(telemetry, activity, theme);
    frame.render_widget(Paragraph::new(lines).style(theme.panel), details);
    render_context_gauge(frame, gauge, telemetry, activity, theme);
}

fn render_context_gauge(
    frame: &mut Frame<'_>,
    area: Rect,
    telemetry: &TelemetryView<'_>,
    activity: ActivityState,
    theme: Theme,
) {
    if area.is_empty() {
        return;
    }
    let pending = metrics_pending(activity);
    let ratio = if pending {
        0.0
    } else {
        telemetry.context_ratio().unwrap_or(0.0).clamp(0.0, 1.0)
    };
    let warning = context_is_high(telemetry, activity);
    let label = context_gauge_label(telemetry, activity);
    let style = if warning {
        theme.warning
    } else if pending {
        theme.secondary
    } else {
        theme.accent
    };
    let gauge = LineGauge::default()
        .filled_style(style)
        .unfilled_style(theme.muted)
        .label(Line::styled(label, style))
        .ratio(ratio);
    let gauge = if theme.ascii {
        gauge.filled_symbol("=").unfilled_symbol("-")
    } else {
        gauge
    };
    frame.render_widget(gauge, area);
}

fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &UiView<'_, '_>,
    theme: Theme,
    class: LayoutClass,
) {
    if area.is_empty() {
        return;
    }
    let telemetry = view.telemetry;
    let model = nonempty_or(telemetry.model, "NO MODEL");
    let pending = metrics_pending(view.activity);
    let tokens = token_metric(telemetry.generated_tokens, view.activity);
    let token_style = if pending { theme.secondary } else { theme.text };
    let context = context_status(&telemetry, view.activity);
    let context_style = if context_is_high(&telemetry, view.activity) {
        theme.warning
    } else if pending {
        theme.secondary
    } else {
        theme.text
    };

    let line = match class {
        LayoutClass::Wide => Line::from(vec![
            Span::styled(
                " [F3] VOICE  [F4] AUDIO  [Ctrl+K] MODELS  [Ctrl+O] SESSIONS  [F1] HELP  [F2] INSPECT ",
                theme.muted,
            ),
            Span::styled(" // ", theme.border),
            Span::styled(model.to_owned(), theme.accent),
        ]),
        LayoutClass::Compact => Line::from(vec![
            Span::styled(format!(" {model} "), theme.accent),
            Span::styled(
                format!(
                    "// {} {}  TOK ",
                    theme.activity_glyph(view.activity),
                    view.activity.label()
                ),
                theme.text,
            ),
            Span::styled(tokens, token_style),
            Span::styled("  CTX ".to_owned(), theme.text),
            Span::styled(context, context_style),
            Span::styled("  [F2] INSPECT".to_owned(), theme.muted),
        ]),
        LayoutClass::Minimal => Line::from(vec![
            Span::styled(format!(" {model} "), theme.accent),
            Span::styled(format!("// {}  TOK ", view.activity.label()), theme.text),
            Span::styled(tokens, token_style),
            Span::styled("  [F1] HELP".to_owned(), theme.muted),
        ]),
        LayoutClass::TooSmall => Line::default(),
    };
    frame.render_widget(Paragraph::new(line).style(theme.panel), area);
}

fn render_too_small(frame: &mut Frame<'_>, area: Rect, theme: Theme) {
    if area.is_empty() {
        return;
    }
    let box_height = cmp::min(area.height, 5);
    let box_width = cmp::min(area.width, 54);
    let notice = Rect::new(
        area.x + area.width.saturating_sub(box_width) / 2,
        area.y + area.height.saturating_sub(box_height) / 2,
        box_width,
        box_height,
    );
    let lines = vec![
        Line::styled("XIKOI // DISPLAY LINK DEGRADED", theme.warning),
        Line::styled(
            format!(
                "terminal {}x{}; need at least 60x18",
                area.width, area.height
            ),
            theme.text,
        ),
        Line::styled("resize to restore the neural interface", theme.muted),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.base)
            .alignment(Alignment::Center),
        notice,
    );
}

fn render_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    overlay: OverlayView<'_>,
    telemetry: &TelemetryView<'_>,
    activity: ActivityState,
    theme: Theme,
) {
    let (title, maximum_height) = match overlay {
        OverlayView::Models { .. } => (" MODEL MATRIX ", 18),
        OverlayView::Sessions { .. } => (" SESSION ARCHIVE ", 18),
        OverlayView::Help { .. } => (" COMMAND LEXICON ", 18),
        OverlayView::Audio { .. } => (" AUDIO MATRIX // CORE AUDIO ", 20),
        OverlayView::Inspector => (" SYSTEM INSPECTOR ", 20),
    };
    let popup = centered_popup(area, 76, maximum_height);
    if popup.is_empty() {
        return;
    }
    frame.render_widget(Clear, popup);
    let block = panel_block(title, theme)
        .border_style(theme.secondary)
        .style(theme.panel);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return;
    }

    match overlay {
        OverlayView::Models {
            items,
            selected,
            query,
        } => render_model_overlay(frame, inner, items, selected, query, theme),
        OverlayView::Sessions { items, selected } => {
            render_session_overlay(frame, inner, items, selected, theme);
        }
        OverlayView::Help { bindings } => render_help_overlay(frame, inner, bindings, theme),
        OverlayView::Audio { items, selected } => {
            render_audio_overlay(frame, inner, items, selected, theme);
        }
        OverlayView::Inspector => render_inspector(frame, inner, telemetry, activity, theme),
    }
}

fn render_audio_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    items: &[AudioChoice<'_>],
    selected: usize,
    theme: Theme,
) {
    let [list_area, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("No host audio devices are currently exposed.")
                .style(theme.warning)
                .alignment(Alignment::Center),
            list_area,
        );
    } else {
        let rows = items.iter().map(|item| {
            let kind = match item.direction {
                AudioDirection::Input => "MIC",
                AudioDirection::Output => "OUT",
            };
            let active = if item.is_selected { "ACTIVE" } else { "      " };
            let default = if item.is_default { " DEFAULT" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(format!("[{kind}] "), theme.accent),
                Span::styled(format!("{:<32}", item.name), theme.text),
                Span::styled(format!(" {active:<6}"), theme.secondary),
                Span::styled(format!("  {}{default}", item.detail), theme.muted),
            ]))
        });
        let list = List::new(rows)
            .highlight_style(theme.selected)
            .highlight_symbol(if theme.ascii { "> " } else { "λ " });
        let mut state = ListState::default();
        state.select(Some(selected.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(list, list_area, &mut state);
    }
    frame.render_widget(
        Paragraph::new("UP/DOWN CHOOSE  ENTER ROUTE  R REFRESH  ESC CLOSE").style(theme.muted),
        footer,
    );
}

fn render_model_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    items: &[ModelChoice<'_>],
    selected: usize,
    query: Option<&str>,
    theme: Theme,
) {
    let [list_area, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(
                "No models available. Load one in the configured chat backend, then Ctrl+R.",
            )
            .style(theme.warning)
            .alignment(Alignment::Center),
            list_area,
        );
    } else {
        let rows = items.iter().map(|item| {
            let unsupported = item.chat_support == Some(false);
            let mut spans = vec![
                Span::styled(
                    format!("[{}] ", item.route.label()),
                    route_style(item.route, theme),
                ),
                Span::styled(
                    item.name.to_owned(),
                    if unsupported { theme.muted } else { theme.text },
                ),
                Span::styled(
                    item.detail
                        .map_or_else(String::new, |detail| format!("  {detail}")),
                    theme.muted,
                ),
            ];
            if unsupported {
                spans.push(Span::styled("  CHAT UNSUPPORTED", theme.warning));
            }
            ListItem::new(Line::from(spans))
        });
        let list = List::new(rows)
            .highlight_style(theme.selected)
            .highlight_symbol(if theme.ascii { "> " } else { "λ " });
        let mut state = ListState::default();
        state.select(Some(selected.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(list, list_area, &mut state);
    }
    let query = query.filter(|value| !value.is_empty()).map_or_else(
        || "UP/DOWN CHOOSE  ENTER SELECT  ESC CLOSE".to_owned(),
        |value| {
            let cursor = if theme.ascii { "_" } else { "█" };
            format!("FILTER // {value}{cursor}  ENTER SELECT  ESC CLOSE")
        },
    );
    frame.render_widget(Paragraph::new(query).style(theme.muted), footer);
}

fn render_session_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    items: &[SessionChoice<'_>],
    selected: usize,
    theme: Theme,
) {
    let [list_area, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("No saved sessions yet.").style(theme.muted),
            list_area,
        );
    } else {
        let rows = items.iter().map(|item| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<18} ", item.title), theme.text),
                Span::styled(format!("{:>3} MSG  ", item.message_count), theme.secondary),
                Span::styled(item.updated.to_owned(), theme.muted),
                Span::styled(format!("  {}", short_id(item.id)), theme.muted),
            ]))
        });
        let list = List::new(rows)
            .highlight_style(theme.selected)
            .highlight_symbol(if theme.ascii { "> " } else { "λ " });
        let mut state = ListState::default();
        state.select(Some(selected.min(items.len().saturating_sub(1))));
        frame.render_stateful_widget(list, list_area, &mut state);
    }
    frame.render_widget(
        Paragraph::new("UP/DOWN CHOOSE  ENTER RESUME  ESC CLOSE").style(theme.muted),
        footer,
    );
}

fn render_help_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    bindings: &[HelpBinding<'_>],
    theme: Theme,
) {
    let [entries_area, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let entries = if bindings.is_empty() {
        default_help(theme)
    } else {
        bindings
            .iter()
            .map(|binding| {
                Line::from(vec![
                    Span::styled(format!("{:>18}", binding.keys), theme.accent),
                    Span::styled("  //  ".to_owned(), theme.muted),
                    Span::styled(binding.action.to_owned(), theme.text),
                ])
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(entries)
            .style(theme.panel)
            .alignment(Alignment::Left),
        entries_area,
    );
    frame.render_widget(Paragraph::new("F1 / ESC CLOSE").style(theme.muted), footer);
}

fn render_inspector(
    frame: &mut Frame<'_>,
    area: Rect,
    telemetry: &TelemetryView<'_>,
    activity: ActivityState,
    theme: Theme,
) {
    let [details, gauge, footer] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let lines = telemetry_detail_lines(telemetry, activity, theme);
    frame.render_widget(Paragraph::new(lines).style(theme.panel), details);
    render_context_gauge(frame, gauge, telemetry, activity, theme);
    let footer_text = if metrics_pending(activity) {
        "ESC CLOSE  //  token metrics pending the backend's final stream record"
    } else {
        "ESC CLOSE  //  exact values from the backend's final stream record"
    };
    frame.render_widget(Paragraph::new(footer_text).style(theme.muted), footer);
}

fn panel_block<'a>(title: impl Into<Line<'a>>, theme: Theme) -> Block<'a> {
    Block::new()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .border_style(theme.border)
        .style(theme.panel)
        .title(title)
}

fn route_style(route: RouteLocation, theme: Theme) -> ratatui::style::Style {
    match route {
        RouteLocation::Loopback => theme.accent,
        RouteLocation::Disabled => theme.muted,
        RouteLocation::Network | RouteLocation::Cloud | RouteLocation::Unknown => theme.warning,
    }
}

fn redact_endpoint(endpoint: &str) -> String {
    if let Ok(url) = Url::parse(endpoint) {
        return crate::config::redact_endpoint(&url);
    }
    sanitize_terminal_text(endpoint)
}

fn empty_transcript(theme: Theme) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(vec![Span::styled(
            if theme.ascii {
                "XIKOI // SESSION CHANNEL"
            } else {
                "XIKOI // SESSION CHANNEL ◇"
            },
            theme.accent,
        )])
        .centered(),
        Line::styled(
            "Select a model, enter a message, and open the link.",
            theme.text,
        )
        .centered(),
        Line::styled("Your transcript remains on this machine.", theme.muted).centered(),
    ]
}

fn telemetry_summary_lines(
    telemetry: &TelemetryView<'_>,
    activity: ActivityState,
    theme: Theme,
) -> Vec<Line<'static>> {
    let model = nonempty_or(telemetry.model, "NO MODEL");
    let detail = telemetry.model_detail.unwrap_or("--");
    let session = telemetry.session.map_or("--", short_id);
    let prompt = token_metric(telemetry.prompt_tokens, activity);
    let generated = token_metric(telemetry.generated_tokens, activity);
    let loaded = telemetry
        .loaded_size_bytes
        .map_or_else(|| "--".to_owned(), format_bytes);
    let speed = final_metric(activity, || {
        telemetry
            .tokens_per_second
            .map_or_else(|| "--".to_owned(), |value| format!("{value:.1}"))
    });
    let first = final_metric(activity, || {
        telemetry
            .time_to_first_token_ms
            .map_or_else(|| "--".to_owned(), |value| format!("{value}ms"))
    });
    let load_duration = final_metric(activity, || {
        telemetry
            .load_duration_ms
            .map_or_else(|| "--".to_owned(), format_millis)
    });
    let total_duration = final_metric(activity, || {
        telemetry
            .total_duration_ms
            .map_or_else(|| "--".to_owned(), format_millis)
    });
    let final_value_style = if metrics_pending(activity) {
        theme.secondary
    } else {
        theme.text
    };

    vec![
        metric_line("MODEL", model, theme.accent, theme),
        metric_line("BUILD", detail, theme.text, theme),
        metric_line("SESSION", session, theme.text, theme),
        Line::from(vec![
            Span::styled("MEMORY  ", theme.muted),
            Span::styled(loaded, theme.text),
            Span::styled(
                telemetry
                    .maximum_context
                    .map_or_else(String::new, |value| format!("  MAXCTX {value}")),
                theme.muted,
            ),
        ]),
        Line::from(vec![
            Span::styled("TOKENS  ", theme.muted),
            Span::styled(format!("P {prompt} / O {generated}"), final_value_style),
        ]),
        Line::from(vec![
            Span::styled("RATE    ", theme.muted),
            Span::styled(format!("{speed} tok/s  TTFT {first}"), theme.secondary),
        ]),
        Line::from(vec![
            Span::styled("TIME    ", theme.muted),
            Span::styled(
                format!("LOAD {load_duration}  TOTAL {total_duration}"),
                theme.text,
            ),
        ]),
    ]
}

fn telemetry_detail_lines(
    telemetry: &TelemetryView<'_>,
    activity: ActivityState,
    theme: Theme,
) -> Vec<Line<'static>> {
    let endpoint = redact_endpoint(nonempty_or(telemetry.endpoint, "--"));
    let elapsed = telemetry
        .elapsed_ms
        .map_or_else(|| "--".to_owned(), format_millis);
    let backend = telemetry.backend_version.unwrap_or("--");
    let backend_name = nonempty_or(telemetry.backend_name, "BACKEND");
    let session = telemetry.session.unwrap_or("--");
    let allocated_context = optional_u64(telemetry.context_limit);
    let maximum_context = optional_u64(telemetry.maximum_context);
    let loaded_size = telemetry
        .loaded_size_bytes
        .map_or_else(|| "--".to_owned(), format_bytes);
    let prompt = token_metric(telemetry.prompt_tokens, activity);
    let generated = token_metric(telemetry.generated_tokens, activity);
    let speed = final_metric(activity, || {
        telemetry
            .tokens_per_second
            .map_or_else(|| "--".to_owned(), |value| format!("{value:.2} tok/s"))
    });
    let first = final_metric(activity, || {
        telemetry
            .time_to_first_token_ms
            .map_or_else(|| "--".to_owned(), |value| format!("{value} ms"))
    });
    let load_duration = final_metric(activity, || {
        telemetry
            .load_duration_ms
            .map_or_else(|| "--".to_owned(), format_millis)
    });
    let total_duration = final_metric(activity, || {
        telemetry
            .total_duration_ms
            .map_or_else(|| "--".to_owned(), format_millis)
    });
    let final_value_style = if metrics_pending(activity) {
        theme.secondary
    } else {
        theme.text
    };

    vec![
        metric_line(
            "MODEL",
            nonempty_or(telemetry.model, "NO MODEL"),
            theme.accent,
            theme,
        ),
        metric_line(
            "DETAIL",
            telemetry.model_detail.unwrap_or("--"),
            theme.text,
            theme,
        ),
        metric_line(backend_name, backend, theme.text, theme),
        metric_line(
            "SESSION BACKEND",
            telemetry.session_backend,
            theme.text,
            theme,
        ),
        metric_line(
            "CHAT ROUTE",
            telemetry.chat_route.label(),
            route_style(telemetry.chat_route, theme),
            theme,
        ),
        metric_line(
            "SPEECH ROUTE",
            telemetry.speech_route.label(),
            route_style(telemetry.speech_route, theme),
            theme,
        ),
        metric_line("ENDPOINT", &endpoint, theme.secondary, theme),
        metric_line("SESSION", session, theme.text, theme),
        metric_line("ALLOC CONTEXT", &allocated_context, theme.text, theme),
        metric_line("MAX CONTEXT", &maximum_context, theme.text, theme),
        metric_line("LOADED SIZE", &loaded_size, theme.text, theme),
        metric_line("PROMPT TOKENS", &prompt, final_value_style, theme),
        metric_line("OUTPUT TOKENS", &generated, final_value_style, theme),
        metric_line("OUTPUT RATE", &speed, theme.secondary, theme),
        metric_line("FIRST TOKEN", &first, theme.text, theme),
        metric_line("LOAD TIME", &load_duration, theme.text, theme),
        metric_line("TOTAL TIME", &total_duration, theme.text, theme),
        metric_line("LIVE ELAPSED", &elapsed, theme.text, theme),
    ]
}

fn metric_line(
    label: &str,
    value: &str,
    value_style: ratatui::style::Style,
    theme: Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<14}"), theme.muted),
        Span::styled(value.to_owned(), value_style),
    ])
}

fn default_help(theme: Theme) -> Vec<Line<'static>> {
    [
        ("Enter", "send message"),
        ("Alt+Enter / Ctrl+N", "insert newline"),
        ("Esc", "close overlay / cancel generation"),
        ("PgUp / PgDn", "scroll transcript"),
        ("End", "follow newest output"),
        ("Ctrl+K", "choose local model"),
        ("Ctrl+O", "open session archive"),
        ("Ctrl+L", "start new session"),
        ("Ctrl+R", "retry Ollama connection"),
        ("F1", "toggle this command lexicon"),
        ("F2", "open system inspector"),
        ("Ctrl+C", "save and quit"),
    ]
    .into_iter()
    .map(|(keys, action)| {
        Line::from(vec![
            Span::styled(format!("{keys:>18}"), theme.accent),
            Span::styled("  //  ".to_owned(), theme.muted),
            Span::styled(action.to_owned(), theme.text),
        ])
    })
    .collect()
}

fn ascii_signal(samples: &[u64], width: usize) -> String {
    const LEVELS: &[u8] = b" .:-=+*#%@";

    if width == 0 {
        return String::new();
    }
    let start = samples.len().saturating_sub(width);
    let visible = &samples[start..];
    let maximum = visible.iter().copied().max().unwrap_or(1).max(1);
    let mut output = String::with_capacity(width);
    for _ in 0..width.saturating_sub(visible.len()) {
        output.push(' ');
    }
    for value in visible {
        let index =
            ((u128::from(*value) * (LEVELS.len() - 1) as u128) / u128::from(maximum)) as usize;
        output.push(char::from(LEVELS[index]));
    }
    output
}

fn centered_popup(area: Rect, maximum_width: u16, maximum_height: u16) -> Rect {
    let available_width = area.width.saturating_sub(4);
    let available_height = area.height.saturating_sub(4);
    let width = maximum_width.min(available_width);
    let height = maximum_height.min(available_height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn inset_horizontally(area: Rect, amount: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(amount.min(area.width)),
        area.y,
        area.width.saturating_sub(amount.saturating_mul(2)),
        area.height,
    )
}

fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "--".to_owned(), |value| value.to_string())
}

const fn metrics_pending(activity: ActivityState) -> bool {
    matches!(
        activity,
        ActivityState::Thinking | ActivityState::Streaming | ActivityState::Cancelling
    )
}

const fn metrics_unavailable(activity: ActivityState) -> bool {
    matches!(activity, ActivityState::Failed)
}

fn final_metric(activity: ActivityState, exact: impl FnOnce() -> String) -> String {
    if metrics_pending(activity) {
        "pending".to_owned()
    } else if metrics_unavailable(activity) {
        "--".to_owned()
    } else {
        exact()
    }
}

fn token_metric(value: Option<u64>, activity: ActivityState) -> String {
    final_metric(activity, || optional_u64(value))
}

fn context_is_high(telemetry: &TelemetryView<'_>, activity: ActivityState) -> bool {
    !metrics_pending(activity)
        && !metrics_unavailable(activity)
        && telemetry
            .context_ratio()
            .is_some_and(|ratio| ratio >= CONTEXT_WARNING_RATIO)
}

fn context_gauge_label(telemetry: &TelemetryView<'_>, activity: ActivityState) -> String {
    if metrics_pending(activity) {
        return telemetry.context_limit.map_or_else(
            || "CTX PENDING".to_owned(),
            |limit| format!("CTX PENDING // ALLOC {limit}"),
        );
    }
    if metrics_unavailable(activity) {
        return telemetry
            .context_limit
            .map_or_else(|| "CTX --".to_owned(), |limit| format!("CTX --/{limit}"));
    }

    match (
        telemetry.context_used,
        telemetry.context_limit,
        telemetry.context_ratio(),
    ) {
        (Some(used), Some(limit), Some(ratio)) => {
            let warning = if context_is_high(telemetry, activity) {
                " HIGH"
            } else {
                ""
            };
            let maximum = telemetry
                .maximum_context
                .map_or_else(String::new, |maximum| format!("  MAX {maximum}"));
            format!(
                "CTX{warning} {used}/{limit}  {:.0}%{maximum}",
                ratio * 100.0
            )
        }
        (_, Some(limit), _) => format!("CTX --/{limit}"),
        _ => "CTX --".to_owned(),
    }
}

fn context_status(telemetry: &TelemetryView<'_>, activity: ActivityState) -> String {
    if metrics_pending(activity) {
        return telemetry
            .context_limit
            .map_or_else(|| "pending".to_owned(), |limit| format!("pending/{limit}"));
    }
    if metrics_unavailable(activity) {
        return telemetry
            .context_limit
            .map_or_else(|| "--".to_owned(), |limit| format!("--/{limit}"));
    }

    match (telemetry.context_used, telemetry.context_limit) {
        (Some(used), Some(limit)) if context_is_high(telemetry, activity) => {
            format!("{used}/{limit} HIGH")
        }
        (Some(used), Some(limit)) => format!("{used}/{limit}"),
        (None, Some(limit)) => format!("--/{limit}"),
        _ => "--".to_owned(),
    }
}

fn nonempty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

fn short_id(value: &str) -> &str {
    value.get(..cmp::min(value.len(), 8)).unwrap_or(value)
}

fn format_millis(value: u64) -> String {
    let mut output = String::new();
    if value >= 60_000 {
        let _ = write!(
            output,
            "{}m {:02}.{:01}s",
            value / 60_000,
            (value % 60_000) / 1_000,
            (value % 1_000) / 100
        );
    } else {
        let _ = write!(output, "{}.{:01}s", value / 1_000, (value % 1_000) / 100);
    }
    output
}

fn format_bytes(value: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if value >= GIB {
        fixed_unit(value, GIB, 100, "GiB")
    } else if value >= MIB {
        fixed_unit(value, MIB, 10, "MiB")
    } else {
        format!("{value} B")
    }
}

fn fixed_unit(value: u64, unit: u64, scale: u64, suffix: &str) -> String {
    let whole = value / unit;
    let mut fraction = ((value % unit) * scale + unit / 2) / unit;
    let whole = if fraction == scale {
        fraction = 0;
        whole.saturating_add(1)
    } else {
        whole
    };
    let decimals = scale.ilog10() as usize;
    format!("{whole}.{fraction:0decimals$} {suffix}")
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use ratatui::{Terminal, backend::TestBackend, layout::Size};
    use ratatui_textarea::TextArea;
    use uuid::Uuid;

    use crate::{
        audio::AudioDirection,
        config::ColorMode,
        domain::{ChatMessage, MessageOrigin, MessageStatus, Role},
        ui::{
            ActivityState, AudioChoice, HelpBinding, InteractionMode, LinkState, ModelChoice,
            OverlayView, SessionChoice, TelemetryView, ThemeOptions, TranscriptScroll, UiView,
            VoiceState, VoiceView,
        },
    };

    use super::*;

    static MODELS: [ModelChoice<'static>; 2] = [
        ModelChoice {
            name: "qwen3.5:9b",
            detail: Some("9B Q4_K_M"),
            chat_support: Some(true),
            route: RouteLocation::Loopback,
        },
        ModelChoice {
            name: "remote-reasoner",
            detail: Some("cloud"),
            chat_support: Some(true),
            route: RouteLocation::Cloud,
        },
    ];

    #[test]
    fn breakpoints_are_exact_on_both_axes() {
        assert_eq!(layout_class(Size::new(110, 32)), LayoutClass::Wide);
        assert_eq!(layout_class(Size::new(109, 32)), LayoutClass::Compact);
        assert_eq!(layout_class(Size::new(110, 31)), LayoutClass::Compact);
        assert_eq!(layout_class(Size::new(80, 24)), LayoutClass::Compact);
        assert_eq!(layout_class(Size::new(79, 24)), LayoutClass::Minimal);
        assert_eq!(layout_class(Size::new(80, 23)), LayoutClass::Minimal);
        assert_eq!(layout_class(Size::new(60, 18)), LayoutClass::Minimal);
        assert_eq!(layout_class(Size::new(59, 18)), LayoutClass::TooSmall);
        assert_eq!(layout_class(Size::new(60, 17)), LayoutClass::TooSmall);
    }

    #[test]
    fn requested_size_snapshot_matrix() {
        insta::assert_snapshot!(
            "wide_120x40_finished",
            rendered(120, 40, |view| {
                view.activity = ActivityState::Idle;
                view.status_message = Some("LINK COMPLETE");
            })
        );
        insta::assert_snapshot!("compact_90x28_streaming", rendered(90, 28, |_| {}));
        insta::assert_snapshot!(
            "minimal_60x18_offline_ascii",
            rendered(60, 18, |view| {
                view.theme = ThemeOptions::new(ColorMode::Mono).with_ascii(true);
                view.link = LinkState::Offline;
                view.activity = ActivityState::Failed;
                view.status_message = Some("OLLAMA OFFLINE");
            })
        );
        insta::assert_snapshot!("below_min_59x17", rendered(59, 17, |_| {}));
    }

    #[test]
    fn requested_wide_layout_has_a_dominant_visualizer_and_readable_telemetry() {
        let output = rendered(120, 40, |view| {
            view.activity = ActivityState::Idle;
        });
        assert!(output.contains("LINK ACTIVITY"));
        assert!(output.contains("TRANSCRIPT"));
        assert!(output.contains("COMPOSE"));
        assert!(output.contains("SYSTEM TELEMETRY"));
        assert!(output.contains("qwen3.5:9b"));
        assert!(output.contains("5.00 GiB"));
        assert!(output.contains("MAXCTX 32768"));
        assert!(output.contains("P 1024 / O 128"));
        assert!(output.contains("LOAD 0.6s  TOTAL 3.4s"));
        assert!(output.contains("[Ctrl+K] MODELS"));
        assert!(output.contains("[Ctrl+O] SESSIONS"));
        assert!(output.contains("[F1] HELP"));
        assert!(output.contains("[F2] INSPECT"));

        let visualizer_rows = output
            .lines()
            .take(8)
            .filter(|line| line.chars().any(|character| "▁▂▃▄▅▆▇█".contains(character)))
            .count();
        assert!(
            visualizer_rows >= 3,
            "wide neural visualizer should occupy multiple rows"
        );
    }

    #[test]
    fn requested_compact_layout_replaces_telemetry_pane_with_status_bar() {
        let output = rendered(90, 28, |_| {});
        assert!(output.contains("LINK ACTIVITY"));
        assert!(output.contains("TRANSCRIPT"));
        assert!(output.contains("COMPOSE"));
        assert!(!output.contains("SYSTEM TELEMETRY"));
        assert!(output.contains("TOK pending"));
        assert!(output.contains("[F2] INSPECT"));
        assert!(!output.contains("[I] INSPECT"));
    }

    #[test]
    fn requested_minimal_layout_keeps_chat_operable_without_decorative_signals() {
        let output = rendered(60, 18, |view| {
            view.theme = ThemeOptions::new(ColorMode::Mono).with_ascii(true);
            view.link_signal_samples = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
            view.input_signal_samples = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        });
        assert!(output.contains("LINK ONLINE"));
        assert!(output.contains("ACTIVITY STREAMING"));
        assert!(output.contains("TRANSCRIPT"));
        assert!(output.contains("COMPOSE"));
        assert!(output.contains("[F1] HELP"));
        assert!(!output.contains("KEY ECHO"));
        assert!(!output.contains(".:-=+*#%@"));
    }

    #[test]
    fn too_small_layout_is_safe_and_actionable() {
        let output = rendered(40, 10, |_| {});
        assert!(output.contains("DISPLAY LINK DEGRADED"));
        assert!(output.contains("need at least 60x18"));

        // The smallest useful TestBackend should still render without a panic.
        let _ = rendered(1, 1, |_| {});
    }

    #[test]
    fn transcript_scrollbar_and_streaming_state_are_visible() {
        let output = rendered(90, 28, |view| {
            view.theme = ThemeOptions::new(ColorMode::Mono).with_ascii(true);
            view.transcript_scroll = TranscriptScroll {
                offset_from_tail: 2,
                follow_tail: false,
                anchor: None,
            };
        });
        assert!(output.contains("TRANSCRIPT"));
        assert!(output.contains("STREAMING"));
        assert!(output.contains('#'));
    }

    #[test]
    fn unicode_mode_uses_a_unicode_transcript_scrollbar() {
        let output = rendered(90, 28, |view| {
            view.link_signal_samples = &[];
            view.input_signal_samples = &[];
        });
        assert!(output.contains('█'));
    }

    #[test]
    fn activity_signal_is_driven_by_supplied_samples_in_ascii_mode() {
        let output = rendered(90, 28, |view| {
            view.theme = ThemeOptions::new(ColorMode::Mono).with_ascii(true);
            view.link_signal_samples = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        });
        assert!(output.contains(".:-=+*#%@"));
    }

    #[test]
    fn composer_has_an_independent_input_activity_signal() {
        let output = rendered(90, 28, |view| {
            view.theme = ThemeOptions::new(ColorMode::Mono).with_ascii(true);
            view.link_signal_samples = &[];
            view.input_signal_samples = &[0, 1, 2, 7, 9];
        });
        assert!(output.contains("KEY ECHO"));
        assert!(output.contains(".:#@"));
    }

    #[test]
    fn voice_deck_exposes_real_voice_state_and_routes() {
        let output = rendered(120, 40, |view| {
            view.voice = VoiceView {
                mode: InteractionMode::Voice,
                state: VoiceState::Listening,
                speech_online: true,
                input_device: "MacBook Air Microphone",
                output_device: "MacBook Air Speakers",
                asr_model: "qwen-asr",
                tts_model: "qwen-tts",
            };
        });
        assert!(output.contains("VOICE USER SPEAKING"));
        assert!(output.contains("NEURAL VOICE"));
        assert!(output.contains("MacBook Air Microphone"));
        assert!(output.contains("MacBook Air Speakers"));
        assert!(output.contains("USER SCOPE"));
        assert!(output.contains("AI SCOPE"));
        assert!(output.contains("VOICE SCOPE // AI"));
        let visualizer = output.lines().take(8).collect::<Vec<_>>().join("\n");
        let deck = output.lines().skip(8).collect::<Vec<_>>().join("\n");
        assert!(
            visualizer.contains("AI SCOPE") && !visualizer.contains("USER SCOPE"),
            "AI scope belongs in the top visualizer:\n{visualizer}"
        );
        assert!(
            deck.contains("USER SCOPE") && !deck.contains("AI SCOPE"),
            "user scope belongs in the voice deck:\n{deck}"
        );
    }

    #[test]
    fn voice_mode_renders_independent_user_and_ai_oscilloscopes() {
        #[allow(clippy::cast_precision_loss)]
        let user: &'static [f32] = Box::leak(
            (0..96)
                .map(|index| (index as f32 * 0.4).sin() * 0.9)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        #[allow(clippy::cast_precision_loss)]
        let ai: &'static [f32] = Box::leak(
            (0..96)
                .map(|index| (index as f32 * 0.55).cos() * 0.7)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        let output = rendered(120, 40, |view| {
            view.voice = VoiceView {
                mode: InteractionMode::Voice,
                state: VoiceState::Speaking,
                speech_online: true,
                input_device: "MacBook Air Microphone",
                output_device: "MacBook Air Speakers",
                asr_model: "qwen-asr",
                tts_model: "qwen-tts",
            };
            view.user_scope_samples = user;
            view.ai_scope_samples = ai;
        });
        assert!(output.contains("USER SCOPE"));
        assert!(output.contains("AI SCOPE"));
        assert!(output.contains("LIVE"));
        assert!(output.contains("VOICE SCOPE // AI"));
        assert!(!output.contains("VOICE SCOPE // USER + AI"));
        let visualizer = output.lines().take(8).collect::<Vec<_>>().join("\n");
        let deck = output.lines().skip(8).collect::<Vec<_>>().join("\n");
        assert!(
            visualizer.contains("AI SCOPE") && !visualizer.contains("USER SCOPE"),
            "top visualizer should show only the AI oscilloscope:\n{visualizer}"
        );
        assert!(
            deck.contains("USER SCOPE") && !deck.contains("AI SCOPE"),
            "voice deck should show only the user oscilloscope:\n{deck}"
        );
        let visualizer_rows = visualizer
            .lines()
            .filter(|line| {
                line.chars()
                    .any(|character| !character.is_ascii() || ".:-=+*#%@".contains(character))
            })
            .count();
        assert!(
            visualizer_rows >= 2,
            "AI oscilloscope should occupy the wide visualizer:\n{output}"
        );
    }

    #[test]
    fn all_overlay_states_render_on_the_compact_layout() {
        static SESSIONS: [SessionChoice<'static>; 1] = [SessionChoice {
            id: "019fb1d4-6e31-7d63",
            title: "Rust planning",
            updated: "2m ago",
            message_count: 12,
        }];
        static HELP: [HelpBinding<'static>; 1] = [HelpBinding {
            keys: "Ctrl+K",
            action: "clear composer",
        }];
        static AUDIO: [AudioChoice<'static>; 2] = [
            AudioChoice {
                id: "coreaudio:mic",
                name: "Studio Microphone",
                detail: "1ch · 48.0 kHz",
                direction: AudioDirection::Input,
                is_default: true,
                is_selected: true,
            },
            AudioChoice {
                id: "coreaudio:out",
                name: "Multi-Output Device",
                detail: "2ch · 48.0 kHz",
                direction: AudioDirection::Output,
                is_default: false,
                is_selected: true,
            },
        ];

        let models_output = rendered(90, 28, |view| {
            view.overlay = Some(OverlayView::Models {
                items: &MODELS,
                selected: 0,
                query: Some("qwen"),
            });
        });
        assert!(models_output.contains("MODEL MATRIX"));
        assert!(models_output.contains("qwen3.5:9b"));
        assert!(models_output.contains("FILTER // qwen"));

        let ascii_models_output = rendered(90, 28, |view| {
            view.theme = ThemeOptions::new(ColorMode::Mono).with_ascii(true);
            view.overlay = Some(OverlayView::Models {
                items: &MODELS,
                selected: 0,
                query: Some("qwen"),
            });
        });
        assert!(ascii_models_output.contains("FILTER // qwen_"));
        assert!(!ascii_models_output.contains('█'));

        let no_models_output = rendered(90, 28, |view| {
            view.overlay = Some(OverlayView::Models {
                items: &[],
                selected: 0,
                query: None,
            });
        });
        assert!(no_models_output.contains("No models available"));
        assert!(no_models_output.contains("configured chat backend"));

        let sessions_output = rendered(90, 28, |view| {
            view.overlay = Some(OverlayView::Sessions {
                items: &SESSIONS,
                selected: 0,
            });
        });
        assert!(sessions_output.contains("SESSION ARCHIVE"));
        assert!(sessions_output.contains("Rust planning"));

        let help_output = rendered(90, 28, |view| {
            view.overlay = Some(OverlayView::Help { bindings: &HELP });
        });
        assert!(help_output.contains("COMMAND LEXICON"));
        assert!(help_output.contains("clear composer"));
        assert!(help_output.contains("F1 / ESC CLOSE"));

        let audio_output = rendered(90, 28, |view| {
            view.overlay = Some(OverlayView::Audio {
                items: &AUDIO,
                selected: 1,
            });
        });
        assert!(audio_output.contains("AUDIO MATRIX"));
        assert!(audio_output.contains("Studio Microphone"));
        assert!(audio_output.contains("Multi-Output Device"));

        let inspector_output = rendered(90, 28, |view| {
            view.activity = ActivityState::Idle;
            view.overlay = Some(OverlayView::Inspector);
        });
        assert!(inspector_output.contains("SYSTEM INSPECTOR"));
        assert!(inspector_output.contains("127.0.0.1:11434"));
        assert!(inspector_output.contains("42.50 tok/s"));
    }

    #[test]
    fn offline_and_failed_states_are_textually_exposed() {
        let output = rendered(90, 28, |view| {
            view.link = LinkState::Offline;
            view.activity = ActivityState::Failed;
            view.status_message = Some("connection refused");
        });
        assert!(output.contains("LINK OFFLINE"));
        assert!(output.contains("ACTIVITY ERROR"));
        assert!(output.contains("connection refused"));
    }

    #[test]
    fn every_link_and_activity_state_has_a_text_label() {
        for state in [LinkState::Connecting, LinkState::Online, LinkState::Offline] {
            let output = rendered(90, 28, |view| {
                view.link = state;
                view.status_message = None;
            });
            assert!(output.contains(&format!("LINK {}", state.label())));
        }

        for state in [
            ActivityState::Idle,
            ActivityState::Thinking,
            ActivityState::Streaming,
            ActivityState::Cancelling,
            ActivityState::Failed,
        ] {
            let output = rendered(90, 28, |view| {
                view.activity = state;
                view.status_message = None;
            });
            assert!(output.contains(&format!("ACTIVITY {}", state.label())));
        }
    }

    #[test]
    fn shortcuts_match_the_actual_keyboard_contract() {
        let output = rendered(90, 28, |view| {
            view.overlay = Some(OverlayView::Help { bindings: &[] });
        });
        for shortcut in ["Ctrl+K", "Ctrl+O", "F1", "F2", "PgUp / PgDn"] {
            assert!(output.contains(shortcut), "missing {shortcut}");
        }
        assert!(!output.contains("Up / Down"));
        assert!(!output.contains("[M]"));
        assert!(!output.contains("[S]"));
    }

    #[test]
    fn streaming_metrics_are_pending_and_finished_metrics_are_exact() {
        let streaming = rendered(120, 40, |_| {});
        assert!(streaming.contains("P pending / O pending"));
        assert!(!streaming.contains("P 1024 / O 128"));

        let streaming_inspector = rendered(90, 28, |view| {
            view.overlay = Some(OverlayView::Inspector);
        });
        assert!(streaming_inspector.contains("PROMPT TOKENS pending"));
        assert!(streaming_inspector.contains("OUTPUT TOKENS pending"));
        assert!(!streaming_inspector.contains("OUTPUT TOKENS 128"));

        let finished = rendered(120, 40, |view| {
            view.activity = ActivityState::Idle;
        });
        assert!(finished.contains("P 1024 / O 128"));
        assert!(finished.contains("42.5 tok/s"));
    }

    #[test]
    fn context_warning_activates_at_eighty_percent_of_allocated_context() {
        let below = rendered(120, 40, |view| {
            view.activity = ActivityState::Idle;
            view.telemetry.context_used = Some(79);
            view.telemetry.context_limit = Some(100);
            view.telemetry.maximum_context = Some(100);
        });
        assert!(!below.contains("CTX HIGH"));

        let threshold = rendered(120, 40, |view| {
            view.activity = ActivityState::Idle;
            view.telemetry.context_used = Some(80);
            view.telemetry.context_limit = Some(100);
            view.telemetry.maximum_context = Some(100);
        });
        assert!(threshold.contains("CTX HIGH 80/100"));

        let compact = rendered(90, 28, |view| {
            view.activity = ActivityState::Idle;
            view.telemetry.context_used = Some(90);
            view.telemetry.context_limit = Some(100);
        });
        assert!(compact.contains("CTX 90/100 HIGH"));

        let ascii_gauge = rendered(120, 40, |view| {
            view.theme = ThemeOptions::new(ColorMode::Mono).with_ascii(true);
            view.activity = ActivityState::Idle;
            view.link_signal_samples = &[];
            view.input_signal_samples = &[];
            view.telemetry.context_used = Some(80);
            view.telemetry.context_limit = Some(100);
        });
        let gauge_row = ascii_gauge
            .lines()
            .find(|line| line.contains("CTX HIGH"))
            .expect("context gauge row");
        assert!(gauge_row.contains('='));
        assert!(gauge_row.contains('-'));
    }

    fn rendered(
        width: u16,
        height: u16,
        customize: impl for<'a, 'input> FnOnce(&mut UiView<'a, 'input>),
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let input = TextArea::default();
        let messages = sample_messages();
        let telemetry = TelemetryView {
            model: "qwen3.5:9b",
            model_detail: Some("9B Q4_K_M"),
            backend_name: "OLLAMA",
            session_backend: "OLLAMA",
            endpoint: "http://127.0.0.1:11434",
            chat_route: super::super::RouteLocation::Loopback,
            speech_route: super::super::RouteLocation::Loopback,
            backend_version: Some("0.11.10"),
            session: Some("019fb1d4-6e31-7d63"),
            context_used: Some(1_024),
            context_limit: Some(8_192),
            maximum_context: Some(32_768),
            loaded_size_bytes: Some(5_368_709_120),
            prompt_tokens: Some(1_024),
            generated_tokens: Some(128),
            tokens_per_second: Some(42.5),
            time_to_first_token_ms: Some(183),
            load_duration_ms: Some(620),
            total_duration_ms: Some(3_410),
            elapsed_ms: Some(3_420),
        };
        let signal = [0, 2, 7, 4, 9, 3, 8, 1, 6, 2, 5, 1];
        let mut view = UiView::new(&messages, &input, telemetry);
        view.activity = ActivityState::Streaming;
        view.link_signal_samples = &signal;
        view.input_signal_samples = &signal;
        view.theme = ThemeOptions::new(ColorMode::Truecolor);
        customize(&mut view);
        terminal
            .draw(|frame| render(frame, &view))
            .expect("render succeeds");
        buffer_text(terminal.backend().buffer(), width, height)
    }

    fn sample_messages() -> Vec<ChatMessage> {
        let created_at = Utc
            .with_ymd_and_hms(2026, 7, 30, 12, 0, 0)
            .single()
            .expect("fixed timestamp");
        vec![
            ChatMessage {
                id: Uuid::now_v7(),
                created_at,
                role: Role::User,
                content: "Explain `λ` in **Rust**. 世界 ".repeat(10),
                origin: MessageOrigin::Typed,
                status: MessageStatus::Complete,
                model: None,
                finish_reason: None,
                usage: None,
                error: None,
            },
            ChatMessage {
                id: Uuid::now_v7(),
                created_at,
                role: Role::Assistant,
                content: "A lambda can be represented by a closure.\n\n```rust\nlet f = |x| x + 1;\n```\n\n"
                    .repeat(5),
                origin: MessageOrigin::Generated,
                status: MessageStatus::Streaming,
                model: Some("qwen3.5:9b".into()),
                finish_reason: None,
                usage: None,
                error: None,
            },
        ]
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
        let mut output = String::new();
        for y in 0..height {
            for x in 0..width {
                output.push_str(buffer[(x, y)].symbol());
            }
            output.push('\n');
        }
        output
    }
}
