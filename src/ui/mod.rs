//! Pure terminal rendering for Xikoi.
//!
//! The application owns all mutable state.  This module receives a borrowed
//! [`UiView`] for each frame and never performs I/O or handles terminal events.

mod markdown;
mod render;
mod sanitize;
mod scope;
mod theme;

use ratatui_textarea::TextArea;

use crate::{audio::AudioDirection, config::ColorMode, domain::ChatMessage};

pub use markdown::{markdown_lines, message_lines, wrap_lines};
pub use render::{LayoutClass, layout_class, page_transcript, render};
pub use sanitize::sanitize_terminal_text;
pub use scope::{ascii_waveform, render_dual_scopes, render_scope, scope_trace};
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InteractionMode {
    #[default]
    Typing,
    Voice,
}

/// Where requests are sent according to the configured endpoint/model.
///
/// `Loopback` describes the URL Xikoi calls; a local proxy may still forward
/// requests elsewhere.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RouteLocation {
    Loopback,
    Network,
    Cloud,
    #[default]
    Unknown,
    Disabled,
}

impl RouteLocation {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Loopback => "LOOPBACK",
            Self::Network => "NETWORK",
            Self::Cloud => "CLOUD",
            Self::Unknown => "UNKNOWN",
            Self::Disabled => "OFF",
        }
    }
}

impl InteractionMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Typing => "TYPE",
            Self::Voice => "VOICE",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VoiceState {
    #[default]
    Disarmed,
    Ready,
    Listening,
    Transcribing,
    Thinking,
    Synthesizing,
    Speaking,
    Failed,
}

impl VoiceState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Disarmed => "DISARMED",
            Self::Ready => "READY",
            Self::Listening => "USER SPEAKING",
            Self::Transcribing => "TRANSCRIBING",
            Self::Thinking => "AI THINKING",
            Self::Synthesizing => "SYNTHESIZING",
            Self::Speaking => "AI SPEAKING",
            Self::Failed => "VOICE ERROR",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoiceView<'a> {
    pub mode: InteractionMode,
    pub state: VoiceState,
    pub speech_online: bool,
    pub input_device: &'a str,
    pub output_device: &'a str,
    pub asr_model: &'a str,
    pub tts_model: &'a str,
}

impl Default for VoiceView<'_> {
    fn default() -> Self {
        Self {
            mode: InteractionMode::Typing,
            state: VoiceState::Disarmed,
            speech_online: false,
            input_device: "SYSTEM DEFAULT",
            output_device: "SYSTEM DEFAULT",
            asr_model: "UNAVAILABLE",
            tts_model: "UNAVAILABLE",
        }
    }
}

/// Stable identity for the row at the top of a detached transcript viewport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranscriptAnchor {
    pub message_id: uuid::Uuid,
    pub row_in_message: usize,
}

/// Scroll state for the transcript.
///
/// Detached views resolve `anchor` against each message's rendered rows, so
/// newly appended messages do not move the reading position. The tail offset
/// remains a fallback for empty transcripts and older callers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TranscriptScroll {
    pub offset_from_tail: usize,
    pub follow_tail: bool,
    pub anchor: Option<TranscriptAnchor>,
}

impl TranscriptScroll {
    pub const fn following_tail() -> Self {
        Self {
            offset_from_tail: 0,
            follow_tail: true,
            anchor: None,
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
    pub backend_name: &'a str,
    pub session_backend: &'a str,
    pub endpoint: &'a str,
    pub chat_route: RouteLocation,
    pub speech_route: RouteLocation,
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
    pub chat_support: Option<bool>,
    pub route: RouteLocation,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioChoice<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub detail: &'a str,
    pub direction: AudioDirection,
    pub is_default: bool,
    pub is_selected: bool,
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
    Audio {
        items: &'a [AudioChoice<'a>],
        selected: usize,
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
    pub voice: VoiceView<'a>,
    /// Rolling Ollama/link activity samples. Values are normalized at render.
    pub link_signal_samples: &'a [u64],
    /// Rolling composer activity samples, such as recent keystroke energy.
    pub input_signal_samples: &'a [u64],
    /// Latest microphone PCM window for the user oscilloscope.
    pub user_scope_samples: &'a [f32],
    /// Latest synthesized-speaker PCM window for the AI oscilloscope.
    pub ai_scope_samples: &'a [f32],
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
            voice: VoiceView::default(),
            link_signal_samples: &[],
            input_signal_samples: &[],
            user_scope_samples: &[],
            ai_scope_samples: &[],
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
            anchor: None,
        };
        assert_eq!(detached.resolved(100, 20), 70);

        let beyond_oldest = TranscriptScroll {
            offset_from_tail: 500,
            follow_tail: false,
            anchor: None,
        };
        assert_eq!(beyond_oldest.resolved(100, 20), 0);

        assert_eq!(TranscriptScroll::following_tail().resolved(100, 20), 80);
    }
}
