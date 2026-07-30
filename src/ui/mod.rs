//! Pure terminal rendering for Xikoi.
//!
//! The application owns all mutable state.  This module receives a borrowed
//! [`UiView`] for each frame and never performs I/O or handles terminal events.

mod markdown;
mod render;
mod theme;

use ratatui_textarea::TextArea;

use crate::{config::ColorMode, domain::ChatMessage};

pub use markdown::{markdown_lines, message_lines, wrap_lines};
pub use render::{LayoutClass, layout_class, render};
pub use theme::{ColorProfile, Theme, ThemeOptions};

/// Current Ollama link state shown in the activity strip.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LinkState {
    Connecting,
    #[default]
    Online,
    Offline,
}

impl LinkState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connecting => "CONNECTING",
            Self::Online => "ONLINE",
            Self::Offline => "OFFLINE",
        }
    }
}

/// Current generation state shown independently from the backend link.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ActivityState {
    #[default]
    Idle,
    Thinking,
    Streaming,
    Cancelling,
    Failed,
}

impl ActivityState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Thinking => "THINKING",
            Self::Streaming => "STREAMING",
            Self::Cancelling => "CANCELLING",
            Self::Failed => "ERROR",
        }
    }
}

/// Scroll state for the transcript.
///
/// `offset_from_tail` is measured in rendered rows above the newest viewport.
/// This lets the app implement `PageUp` by incrementing and `PageDown` by
/// decrementing without knowing the current wrapped transcript height.  When
/// `follow_tail` is set, the renderer ignores the offset and pins to newest.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TranscriptScroll {
    pub offset_from_tail: usize,
    pub follow_tail: bool,
}

impl TranscriptScroll {
    pub const fn following_tail() -> Self {
        Self {
            offset_from_tail: 0,
            follow_tail: true,
        }
    }

    pub const fn resolved(self, content_rows: usize, viewport_rows: usize) -> usize {
        let maximum = content_rows.saturating_sub(viewport_rows);
        if self.follow_tail {
            maximum
        } else {
            maximum.saturating_sub(self.offset_from_tail)
        }
    }
}

/// Values displayed in the wide telemetry pane and compact inspector.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TelemetryView<'a> {
    pub model: &'a str,
    pub model_detail: Option<&'a str>,
    pub endpoint: &'a str,
    pub backend_version: Option<&'a str>,
    pub session: Option<&'a str>,
    pub context_used: Option<u64>,
    /// Context currently allocated to the loaded runner.
    pub context_limit: Option<u64>,
    /// Maximum context advertised by the model metadata.
    pub maximum_context: Option<u64>,
    pub loaded_size_bytes: Option<u64>,
    pub prompt_tokens: Option<u64>,
    pub generated_tokens: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub time_to_first_token_ms: Option<u64>,
    pub load_duration_ms: Option<u64>,
    pub total_duration_ms: Option<u64>,
    /// Live wall-clock duration while a request is still in flight.
    pub elapsed_ms: Option<u64>,
}

impl TelemetryView<'_> {
    #[allow(clippy::cast_precision_loss)]
    pub fn context_ratio(self) -> Option<f64> {
        let used = self.context_used?;
        let limit = self.context_limit?;
        if limit == 0 {
            return None;
        }
        // Terminal gauges accept `f64`; context counters are far below the
        // precision boundary in practice, and the visual ratio is approximate.
        Some((used as f64 / limit as f64).clamp(0.0, 1.0))
    }
}

/// A model row supplied to the model-picker overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelChoice<'a> {
    pub name: &'a str,
    pub detail: Option<&'a str>,
    pub is_cloud: bool,
}

/// A session row supplied to the session-picker overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionChoice<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub updated: &'a str,
    pub message_count: usize,
}

/// A keyboard binding supplied to the help overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelpBinding<'a> {
    pub keys: &'a str,
    pub action: &'a str,
}

/// Modal content drawn above the current layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverlayView<'a> {
    Models {
        items: &'a [ModelChoice<'a>],
        selected: usize,
        query: Option<&'a str>,
    },
    Sessions {
        items: &'a [SessionChoice<'a>],
        selected: usize,
    },
    Help {
        bindings: &'a [HelpBinding<'a>],
    },
    /// Shows the same complete telemetry used by the wide lower-right pane.
    Inspector,
}

/// Everything required to render one frame.
///
/// The two lifetimes are intentionally separate: application data may be
/// borrowed for only the draw call while the textarea can own longer-lived
/// styled content.
#[derive(Debug)]
pub struct UiView<'a, 'input> {
    pub messages: &'a [ChatMessage],
    pub input: &'a TextArea<'input>,
    pub input_title: &'a str,
    pub link: LinkState,
    pub activity: ActivityState,
    pub status_message: Option<&'a str>,
    pub telemetry: TelemetryView<'a>,
    /// Rolling Ollama/link activity samples. Values are normalized at render.
    pub link_signal_samples: &'a [u64],
    /// Rolling composer activity samples, such as recent keystroke energy.
    pub input_signal_samples: &'a [u64],
    pub transcript_scroll: TranscriptScroll,
    pub overlay: Option<OverlayView<'a>>,
    pub theme: ThemeOptions,
}

impl<'a, 'input> UiView<'a, 'input> {
    pub fn new(
        messages: &'a [ChatMessage],
        input: &'a TextArea<'input>,
        telemetry: TelemetryView<'a>,
    ) -> Self {
        Self {
            messages,
            input,
            input_title: "COMPOSE // ENTER SEND  ALT+ENTER NEWLINE",
            link: LinkState::default(),
            activity: ActivityState::default(),
            status_message: None,
            telemetry,
            link_signal_samples: &[],
            input_signal_samples: &[],
            transcript_scroll: TranscriptScroll::following_tail(),
            overlay: None,
            theme: ThemeOptions::new(ColorMode::Auto),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TranscriptScroll;

    #[test]
    fn transcript_offset_is_relative_to_the_newest_viewport() {
        let detached = TranscriptScroll {
            offset_from_tail: 10,
            follow_tail: false,
        };
        assert_eq!(detached.resolved(100, 20), 70);

        let beyond_oldest = TranscriptScroll {
            offset_from_tail: 500,
            follow_tail: false,
        };
        assert_eq!(beyond_oldest.resolved(100, 20), 0);

        assert_eq!(TranscriptScroll::following_tail().resolved(100, 20), 80);
    }
}
