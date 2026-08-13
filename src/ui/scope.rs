//! Dual-trace oscilloscope rendering built on [`scope-tui`].
//!
//! The crate's Chart widgets target Ratatui 0.29, so this module uses its
//! public [`Oscilloscope`] settings and [`stream_to_matrix`] helper, then
//! draws the same live/triggered traces with Xikoi's Ratatui 0.30 charts.
//!
//! [`scope-tui`]: https://github.com/alemidev/scope-tui

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    symbols::Marker,
    text::{Line, Span},
    widgets::{Axis, Chart, Dataset, GraphType, Paragraph},
};
use scope::{display::oscilloscope::Oscilloscope, input::stream_to_matrix};

use super::Theme;

/// Convert a mono PCM window into oscilloscope points using `scope-tui`.
#[must_use]
pub fn scope_trace(samples: &[f32], oscilloscope: &Oscilloscope) -> Vec<(f64, f64)> {
    if samples.is_empty() {
        return vec![(0.0, 0.0), (1.0, 0.0)];
    }

    let data = stream_to_matrix(samples.iter().copied().map(f64::from), 1, 1.0);
    let Some(channel) = data.first() else {
        return vec![(0.0, 0.0), (1.0, 0.0)];
    };

    let mut trigger_offset = 0;
    if oscilloscope.triggering {
        let depth = oscilloscope.depth.max(1);
        for index in 0..channel.len() {
            if triggered(
                channel,
                index,
                oscilloscope.threshold,
                depth,
                oscilloscope.falling_edge,
            ) {
                break;
            }
            trigger_offset += 1;
        }
    }

    let points = channel
        .iter()
        .enumerate()
        .filter(|(index, _)| *index >= trigger_offset)
        .map(|(index, sample)| {
            (
                f64::from(u32::try_from(index - trigger_offset).unwrap_or(0)),
                *sample,
            )
        })
        .collect::<Vec<_>>();
    if points.is_empty() {
        vec![(0.0, 0.0), (1.0, 0.0)]
    } else {
        points
    }
}

/// Compact amplitude string used when the deck is too short for a chart.
#[must_use]
pub fn ascii_waveform(samples: &[f32], width: usize) -> String {
    const LEVELS: &[u8] = b" .:-=+*#%@";

    if width == 0 {
        return String::new();
    }
    if samples.is_empty() {
        return " ".repeat(width);
    }

    let peak = samples
        .iter()
        .map(|sample| sample.abs())
        .fold(1e-3_f32, f32::max);
    let mut output = String::with_capacity(width);
    for column in 0..width {
        let start = column.saturating_mul(samples.len()) / width;
        let end = (column.saturating_add(1))
            .saturating_mul(samples.len())
            .div_ceil(width)
            .max(start.saturating_add(1))
            .min(samples.len());
        let magnitude = samples[start..end]
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max);
        let steps = f32::from(u16::try_from(LEVELS.len().saturating_sub(1)).unwrap_or(1));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let index = ((magnitude / peak) * steps) as usize;
        output.push(char::from(LEVELS[index.min(LEVELS.len() - 1)]));
    }
    output
}

/// Draw a single labeled oscilloscope.
pub fn render_scope(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    samples: &[f32],
    style: Style,
    theme: Theme,
) {
    render_named_scope(frame, area, title, samples, style, theme);
}

/// Draw labeled USER and AI traces side by side.
pub fn render_dual_scopes(
    frame: &mut Frame<'_>,
    area: Rect,
    user: &[f32],
    ai: &[f32],
    user_style: Style,
    ai_style: Style,
    theme: Theme,
) {
    if area.is_empty() {
        return;
    }

    if area.width < 20 && area.height >= 4 {
        let [user_area, ai_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(area);
        render_named_scope(frame, user_area, "USER", user, user_style, theme);
        render_named_scope(frame, ai_area, "AI", ai, ai_style, theme);
        return;
    }

    if area.height <= 1 {
        let user_width = usize::from(area.width / 2).saturating_sub(2);
        let ai_width = usize::from(area.width).saturating_sub(user_width + 4);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("U ", theme.muted),
                Span::styled(ascii_waveform(user, user_width), user_style),
                Span::styled(" A ", theme.muted),
                Span::styled(ascii_waveform(ai, ai_width), ai_style),
            ]))
            .style(theme.panel),
            area,
        );
        return;
    }

    let [user_area, ai_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(area);
    render_named_scope(frame, user_area, "USER", user, user_style, theme);
    render_named_scope(frame, ai_area, "AI", ai, ai_style, theme);
}

fn render_named_scope(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    samples: &[f32],
    style: Style,
    theme: Theme,
) {
    if area.is_empty() {
        return;
    }

    if area.height == 1 {
        let width = usize::from(
            area.width
                .saturating_sub(u16::try_from(title.len()).unwrap_or(4) + 1),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{title} "), theme.muted),
                Span::styled(ascii_waveform(samples, width), style),
            ]))
            .style(theme.panel),
            area,
        );
        return;
    }

    let [label, plot] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {title} SCOPE "), theme.muted),
            Span::styled(
                if samples.iter().any(|sample| sample.abs() > 0.01) {
                    "LIVE"
                } else {
                    "IDLE"
                },
                style,
            ),
        ]))
        .style(theme.panel),
        label,
    );
    render_oscilloscope(frame, plot, samples, style, theme);
}

fn render_oscilloscope(
    frame: &mut Frame<'_>,
    area: Rect,
    samples: &[f32],
    style: Style,
    theme: Theme,
) {
    if area.is_empty() {
        return;
    }

    let oscilloscope = Oscilloscope::default();
    let points = scope_trace(samples, &oscilloscope);
    let width = points.last().map_or(1.0, |(x, _)| (*x + 1.0).max(1.0));
    let peak = points
        .iter()
        .map(|(_, sample)| sample.abs())
        .fold(0.0_f64, f64::max);
    let scale = (peak * 1.25).clamp(0.15, 1.0);
    let baseline = [(0.0, 0.0), (width, 0.0)];
    let marker = if theme.ascii {
        Marker::Dot
    } else {
        Marker::Braille
    };
    let datasets = vec![
        Dataset::default()
            .marker(marker)
            .graph_type(GraphType::Line)
            .style(theme.muted)
            .data(&baseline),
        Dataset::default()
            .marker(marker)
            .graph_type(GraphType::Line)
            .style(style)
            .data(&points),
    ];
    frame.render_widget(
        Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, width]))
            .y_axis(Axis::default().bounds([-scale, scale]))
            .legend_position(None)
            .style(theme.panel),
        area,
    );
}

/// Rising/falling trigger copied from `scope-tui` 0.3.5 (`display/oscilloscope.rs`).
fn triggered(data: &[f64], index: usize, threshold: f64, depth: u32, falling_edge: bool) -> bool {
    let depth = usize::try_from(depth).unwrap_or(1);
    if data.len() < index + 1 + depth {
        return false;
    }
    if falling_edge {
        data[index] >= threshold && (1..=depth).all(|offset| data[index + offset] < threshold)
    } else {
        data[index] <= threshold && (1..=depth).all(|offset| data[index + offset] > threshold)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use crate::{config::ColorMode, ui::ThemeOptions};

    use super::*;

    #[test]
    fn crate_stream_to_matrix_separates_channels() {
        let matrix = stream_to_matrix([-1.0_f64, 1.0, -0.5, 0.5].into_iter(), 2, 1.0);
        assert_eq!(matrix[0], vec![-1.0, -0.5]);
        assert_eq!(matrix[1], vec![1.0, 0.5]);
    }

    #[test]
    fn live_trace_preserves_sample_order() {
        let samples = [0.0, 0.25, -0.5, 0.75];
        let points = scope_trace(&samples, &Oscilloscope::default());
        let values = points.iter().map(|(_, sample)| *sample).collect::<Vec<_>>();
        assert_eq!(values, vec![0.0, 0.25, -0.5, 0.75]);
    }

    #[test]
    fn rising_edge_trigger_drops_pre_threshold_samples() {
        let samples = [-0.5, -0.25, 0.25, 0.5];
        let oscilloscope = Oscilloscope {
            triggering: true,
            falling_edge: false,
            threshold: 0.0,
            depth: 1,
            peaks: false,
        };
        let points = scope_trace(&samples, &oscilloscope);
        assert_eq!(
            points,
            vec![(0.0, -0.25), (1.0, 0.25), (2.0, 0.5)],
            "trigger starts at the last sample at or below threshold"
        );
    }

    #[test]
    fn empty_trace_returns_a_baseline() {
        let points = scope_trace(&[], &Oscilloscope::default());
        assert_eq!(points, vec![(0.0, 0.0), (1.0, 0.0)]);
    }

    #[test]
    fn ascii_waveform_fills_the_requested_width() {
        assert_eq!(ascii_waveform(&[], 4), "    ");
        assert_eq!(ascii_waveform(&[0.0, 1.0, 0.0, 1.0], 4).len(), 4);
        assert!(ascii_waveform(&[0.0, 1.0], 8).contains('@'));
    }

    #[test]
    fn dual_scope_render_survives_empty_and_tiny_areas() {
        let theme = Theme::new(ThemeOptions::new(ColorMode::Mono).with_ascii(true));
        for (width, height) in [(0, 0), (1, 1), (8, 2), (20, 1), (40, 8)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|frame| {
                    render_dual_scopes(
                        frame,
                        frame.area(),
                        &[],
                        &[],
                        theme.accent,
                        theme.secondary,
                        theme,
                    );
                })
                .expect("tiny oscilloscope render");
        }
    }

    #[test]
    fn single_scope_render_survives_empty_and_tiny_areas() {
        let theme = Theme::new(ThemeOptions::new(ColorMode::Mono).with_ascii(true));
        for (width, height) in [(0, 0), (1, 1), (8, 2), (20, 1), (40, 8)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|frame| {
                    render_scope(frame, frame.area(), "AI", &[], theme.accent, theme);
                })
                .expect("tiny single oscilloscope render");
        }
    }

    #[test]
    fn sine_wave_produces_visible_plot_cells() {
        let theme = Theme::new(ThemeOptions::new(ColorMode::Truecolor));
        #[allow(clippy::cast_precision_loss)]
        let samples: Vec<f32> = (0..128)
            .map(|index| (index as f32 * 0.35).sin() * 0.85)
            .collect();
        let backend = TestBackend::new(48, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_dual_scopes(
                    frame,
                    frame.area(),
                    &samples,
                    &samples,
                    theme.accent,
                    theme.secondary,
                    theme,
                );
            })
            .expect("sine oscilloscope render");
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..8 {
            for x in 0..48 {
                output.push_str(buffer[(x, y)].symbol());
            }
            output.push('\n');
        }
        assert!(output.contains("USER SCOPE"));
        assert!(output.contains("AI SCOPE"));
        assert!(output.contains("LIVE"));
        assert!(
            output.chars().any(|character| !character.is_ascii()
                || character == '*'
                || character == '.'
                || character == '•'),
            "expected plotted waveform cells, got:\n{output}"
        );
    }
}
