use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, WrapErr};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use ratatui_textarea::{Input, Key, TextArea};
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, interval},
};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::{
    audio::{
        AudioDevice, AudioDirection, AudioInventory, CaptureSession, PlaybackSession,
        QWEN_TTS_SAMPLE_RATE,
    },
    backend::ChatBackend,
    config::{AppStateFile, BackendKind, RuntimeConfig},
    domain::{
        BackendFailure, BackendInfo, ChatEvent, ChatMessage, ChatRequest, GenerationOptions,
        MessageOrigin, MessageStatus, ModelSummary, SelectedModel, Usage,
    },
    speech::{
        MlxAudioClient, SpeechFailure, SpeechServerInfo, SynthesisRequest, TranscriptionRequest,
    },
    storage::{LoadedSession, SessionSummary, Storage},
    ui::{
        self, ActivityState, AudioChoice, HelpBinding, InteractionMode, LinkState, ModelChoice,
        OverlayView, SessionChoice, TelemetryView, ThemeOptions, TranscriptScroll, UiView,
        VoiceState, VoiceView,
    },
};

const EVENT_CHANNEL_CAPACITY: usize = 256;
const SIGNAL_CAPACITY: usize = 160;
const PAGE_ROWS: usize = 8;
const OFFLINE_RETRY: Duration = Duration::from_secs(5);
const CONTEXT_WARNING_RATIO: f64 = 0.8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Overlay {
    Models,
    Sessions,
    Help,
    Inspector,
    Audio,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupModelChoice {
    Select(usize),
    RequestedUnavailable,
    NoModels,
    Picker,
}

enum InternalEvent {
    BackendReady {
        info: BackendInfo,
        models: Vec<ModelSummary>,
    },
    BackendOffline(BackendFailure),
    ModelHydrated(Box<ModelHydration>),
    Chat(ChatEvent),
    SpeechProbe(Result<SpeechServerInfo, String>),
    Transcription(Result<String, String>),
    TtsChunk(Vec<u8>),
    TtsFinished(Result<(), String>),
}

struct ModelHydration {
    name: String,
    result: Result<SelectedModel, BackendFailure>,
    running: Result<Vec<SelectedModel>, BackendFailure>,
}

/// The single owner of terminal-facing and conversation state.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    config: RuntimeConfig,
    storage: Storage,
    backend: Arc<dyn ChatBackend>,
    speech: Arc<MlxAudioClient>,
    persisted: AppStateFile,
    session: LoadedSession,
    sessions: Vec<SessionSummary>,
    models: Vec<ModelSummary>,
    selected_model: Option<SelectedModel>,
    backend_info: Option<BackendInfo>,
    input: TextArea<'static>,
    link: LinkState,
    activity: ActivityState,
    voice_mode: bool,
    voice_state: VoiceState,
    speech_online: bool,
    audio_inventory: AudioInventory,
    capture: Option<CaptureSession>,
    playback: Option<PlaybackSession>,
    voice_cancel: Option<CancellationToken>,
    tts_response_finished: bool,
    status: Option<String>,
    overlay: Option<Overlay>,
    overlay_index: usize,
    transcript_scroll: TranscriptScroll,
    signal_samples: VecDeque<u64>,
    input_signal_samples: VecDeque<u64>,
    last_usage: Option<Usage>,
    streaming_message: Option<usize>,
    generation_started: Option<Instant>,
    generation_cancel: Option<CancellationToken>,
    backend_refreshing: bool,
    speech_refreshing: bool,
    last_backend_attempt: Instant,
    last_speech_attempt: Instant,
    internal_tx: mpsc::Sender<InternalEvent>,
    internal_rx: mpsc::Receiver<InternalEvent>,
    theme: ThemeOptions,
    should_quit: bool,
    dirty: bool,
}

impl App {
    pub async fn bootstrap(
        config: RuntimeConfig,
        storage: Storage,
        backend: Arc<dyn ChatBackend>,
    ) -> Result<Self> {
        let mut persisted = storage.load_state().await?;
        let sessions = storage.list_sessions().await?;
        let requested_session = config
            .resume
            .as_deref()
            .or(persisted.recent_session.as_deref())
            .and_then(|value| Uuid::parse_str(value).ok());

        let (session, initial_status) = if let Some(id) = requested_session {
            match storage.load_session(id).await {
                Ok(session) => {
                    let status = session
                        .had_interruption
                        .then(|| "RECOVERED // previous turn was interrupted".to_owned());
                    (session, status)
                }
                Err(error) => {
                    warn!(%error, "could not resume requested chat; creating a new one");
                    (
                        storage.create_session().await?,
                        Some("SESSION NOT FOUND // opened a new link".to_owned()),
                    )
                }
            }
        } else {
            (storage.create_session().await?, None)
        };

        persisted.recent_session = Some(session.id.to_string());
        storage.save_state(&persisted).await?;
        let (internal_tx, internal_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let speech = Arc::new(
            MlxAudioClient::new(config.speech.url.clone())
                .wrap_err("could not create MLX-Audio client")?,
        );
        let audio_inventory = AudioInventory::discover().unwrap_or_else(|error| {
            warn!(%error, "could not enumerate host audio devices");
            AudioInventory::default()
        });
        let voice_mode = config.speech.enabled && persisted.voice_mode;
        let mut input = TextArea::default();
        configure_input(&mut input);

        Ok(Self {
            theme: ThemeOptions::from_env(config.color),
            config,
            storage,
            backend,
            speech,
            persisted,
            session,
            sessions,
            models: Vec::new(),
            selected_model: None,
            backend_info: None,
            input,
            link: LinkState::Connecting,
            activity: ActivityState::Idle,
            voice_mode,
            voice_state: VoiceState::Disarmed,
            speech_online: false,
            audio_inventory,
            capture: None,
            playback: None,
            voice_cancel: None,
            tts_response_finished: false,
            status: initial_status,
            overlay: None,
            overlay_index: 0,
            transcript_scroll: TranscriptScroll::following_tail(),
            signal_samples: VecDeque::from(vec![0; SIGNAL_CAPACITY]),
            input_signal_samples: VecDeque::from(vec![0; SIGNAL_CAPACITY]),
            last_usage: None,
            streaming_message: None,
            generation_started: None,
            generation_cancel: None,
            backend_refreshing: false,
            speech_refreshing: false,
            last_backend_attempt: Instant::now()
                .checked_sub(OFFLINE_RETRY)
                .unwrap_or_else(Instant::now),
            last_speech_attempt: Instant::now()
                .checked_sub(OFFLINE_RETRY)
                .unwrap_or_else(Instant::now),
            internal_tx,
            internal_rx,
            should_quit: false,
            dirty: true,
        })
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut terminal_events = EventStream::new();
        let tick_rate = if self.config.animation {
            Duration::from_micros(66_667)
        } else {
            Duration::from_secs(1)
        };
        let mut ticker = interval(tick_rate);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let shutdown = tokio::signal::ctrl_c();
        tokio::pin!(shutdown);
        self.spawn_backend_refresh();
        self.spawn_speech_probe();

        while !self.should_quit {
            if self.dirty {
                terminal
                    .draw(|frame| self.render(frame))
                    .wrap_err("terminal rendering failed")?;
                self.dirty = false;
            }

            tokio::select! {
                terminal_event = terminal_events.next() => {
                    match terminal_event {
                        Some(Ok(event)) => self.handle_terminal_event(event).await?,
                        Some(Err(error)) => return Err(error).wrap_err("terminal event stream failed"),
                        None => self.request_quit().await?,
                    }
                }
                internal = self.internal_rx.recv() => {
                    if let Some(event) = internal {
                        self.handle_internal_event(event).await?;
                    }
                }
                shutdown_result = &mut shutdown => {
                    shutdown_result.wrap_err("could not listen for shutdown signal")?;
                    self.request_quit().await?;
                }
                _ = ticker.tick() => self.handle_tick(),
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn render(&self, frame: &mut ratatui::Frame<'_>) {
        let detail = self.selected_model.as_ref().and_then(model_detail);
        let session_id = short_id(self.session.id);
        let telemetry = self.telemetry_view(&session_id, detail.as_deref());

        let model_details: Vec<_> = self
            .models
            .iter()
            .map(|model| {
                [
                    model.details.parameter_size.as_deref(),
                    model.details.quantization_level.as_deref(),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ")
            })
            .collect();
        let model_choices: Vec<_> = self
            .models
            .iter()
            .zip(&model_details)
            .map(|(model, detail)| ModelChoice {
                name: &model.name,
                detail: (!detail.is_empty()).then_some(detail.as_str()),
                is_cloud: model.is_cloud(),
            })
            .collect();

        let session_ids: Vec<_> = self.sessions.iter().map(|item| short_id(item.id)).collect();
        let session_dates: Vec<_> = self
            .sessions
            .iter()
            .map(|item| item.modified_at.format("%Y-%m-%d %H:%M").to_string())
            .collect();
        let session_choices: Vec<_> = self
            .sessions
            .iter()
            .zip(&session_ids)
            .zip(&session_dates)
            .map(|((session, id), date)| SessionChoice {
                id,
                title: &session.title,
                updated: date,
                message_count: session.message_count,
            })
            .collect();
        let audio_devices = self
            .audio_inventory
            .inputs
            .iter()
            .chain(self.audio_inventory.outputs.iter())
            .collect::<Vec<_>>();
        let audio_details = audio_devices
            .iter()
            .map(|device| {
                format!(
                    "{}ch · {:.1} kHz",
                    device.channels,
                    f64::from(device.sample_rate) / 1000.0
                )
            })
            .collect::<Vec<_>>();
        let audio_choices = audio_devices
            .iter()
            .zip(&audio_details)
            .map(|(device, detail)| AudioChoice {
                id: &device.id,
                name: &device.name,
                detail,
                direction: device.direction,
                is_default: device.is_default,
                is_selected: match device.direction {
                    AudioDirection::Input => self
                        .persisted
                        .input_device_id
                        .as_deref()
                        .map_or(device.is_default, |id| id == device.id),
                    AudioDirection::Output => self
                        .persisted
                        .output_device_id
                        .as_deref()
                        .map_or(device.is_default, |id| id == device.id),
                },
            })
            .collect::<Vec<_>>();
        let link_signal_samples: Vec<_> = if self.config.animation {
            self.signal_samples.iter().copied().collect()
        } else {
            Vec::new()
        };
        let input_signal_samples: Vec<_> = if self.config.animation {
            self.input_signal_samples.iter().copied().collect()
        } else {
            Vec::new()
        };
        let help = help_bindings();
        let overlay = match self.overlay {
            Some(Overlay::Models) => Some(OverlayView::Models {
                items: &model_choices,
                selected: self.overlay_index,
                query: None,
            }),
            Some(Overlay::Sessions) => Some(OverlayView::Sessions {
                items: &session_choices,
                selected: self.overlay_index,
            }),
            Some(Overlay::Help) => Some(OverlayView::Help { bindings: &help }),
            Some(Overlay::Inspector) => Some(OverlayView::Inspector),
            Some(Overlay::Audio) => Some(OverlayView::Audio {
                items: &audio_choices,
                selected: self.overlay_index,
            }),
            None => None,
        };

        let input_device = selected_device_name(
            &self.audio_inventory.inputs,
            self.persisted.input_device_id.as_deref(),
        );
        let output_device = selected_device_name(
            &self.audio_inventory.outputs,
            self.persisted.output_device_id.as_deref(),
        );
        let voice = VoiceView {
            mode: if self.voice_mode {
                InteractionMode::Voice
            } else {
                InteractionMode::Typing
            },
            state: self.voice_state,
            speech_online: self.speech_online,
            input_device,
            output_device,
            asr_model: &self.config.speech.asr_model,
            tts_model: &self.config.speech.tts_model,
        };
        let input_title = if self.voice_mode {
            "VOICE DECK // SPACE RECORD/STOP  F3 TYPE  F4 AUDIO"
        } else {
            "SIGNAL INPUT // ENTER SEND  ALT+ENTER NEWLINE  F3 VOICE"
        };

        let view = UiView {
            messages: &self.session.messages,
            input: &self.input,
            input_title,
            link: self.link,
            activity: self.activity,
            status_message: self.status.as_deref(),
            telemetry,
            voice,
            link_signal_samples: &link_signal_samples,
            input_signal_samples: &input_signal_samples,
            transcript_scroll: self.transcript_scroll,
            overlay,
            theme: self.theme,
        };
        ui::render(frame, &view);
    }

    fn telemetry_view<'a>(
        &'a self,
        session_id: &'a str,
        model_detail: Option<&'a str>,
    ) -> TelemetryView<'a> {
        let endpoint = self.backend_info.as_ref().map_or_else(
            || match self.config.backend {
                BackendKind::Ollama => self.config.ollama_url.as_str(),
                BackendKind::LlamaCpp => self.config.llama_url.as_str(),
            },
            |info| info.endpoint.as_str(),
        );
        let usage = self.last_usage.as_ref();
        let selected = self.selected_model.as_ref();
        let elapsed_ms = self
            .generation_started
            .map(elapsed_millis)
            .or_else(|| usage.and_then(|item| item.total_duration_ns.map(nanos_to_millis)));

        TelemetryView {
            model: selected.map_or("UNSELECTED", |model| model.summary.name.as_str()),
            model_detail,
            endpoint,
            backend_version: self.backend_info.as_ref().map(|info| info.version.as_str()),
            session: Some(session_id),
            context_used: usage.and_then(|item| item.prompt_eval_count),
            context_limit: selected
                .and_then(|model| model.allocated_context.or(model.maximum_context)),
            prompt_tokens: usage.and_then(|item| item.prompt_eval_count),
            generated_tokens: usage.and_then(|item| item.eval_count),
            tokens_per_second: usage.and_then(Usage::output_tokens_per_second),
            time_to_first_token_ms: usage.and_then(|item| item.time_to_first_token_ms),
            elapsed_ms,
            maximum_context: selected.and_then(|model| model.maximum_context),
            loaded_size_bytes: selected.and_then(|model| model.loaded_size),
            load_duration_ms: usage
                .and_then(|item| item.load_duration_ns)
                .map(nanos_to_millis),
            total_duration_ms: usage
                .and_then(|item| item.total_duration_ns)
                .map(nanos_to_millis),
        }
    }

    async fn handle_terminal_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key).await?,
            Event::Paste(text) if self.overlay.is_none() && !self.voice_mode => {
                self.input.insert_str(text);
                self.pulse_input(12);
                self.dirty = true;
            }
            Event::Resize(_, _) => self.dirty = true,
            _ => {}
        }
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.overlay.is_some() {
            return self.handle_overlay_key(key).await;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return self.request_quit().await,
                KeyCode::Char('k') => {
                    self.open_models();
                    return Ok(());
                }
                KeyCode::Char('o') => {
                    self.open_sessions().await?;
                    return Ok(());
                }
                KeyCode::Char('l') => return self.new_session().await,
                KeyCode::Char('n') => {
                    self.input.input(Input {
                        key: Key::Enter,
                        ctrl: false,
                        alt: false,
                        shift: false,
                    });
                    self.pulse_input(16);
                    self.dirty = true;
                    return Ok(());
                }
                KeyCode::Char('r') => {
                    self.spawn_backend_refresh();
                    self.spawn_speech_probe();
                    return Ok(());
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::F(1) => self.open_overlay(Overlay::Help),
            KeyCode::F(2) => self.open_overlay(Overlay::Inspector),
            KeyCode::F(3) => self.toggle_voice_mode().await?,
            KeyCode::F(4) => self.open_audio(),
            KeyCode::Esc => self.cancel_active_work(),
            KeyCode::Char(' ') if self.voice_mode => self.handle_voice_trigger().await?,
            KeyCode::PageUp => {
                self.transcript_scroll.follow_tail = false;
                self.transcript_scroll.offset_from_tail = self
                    .transcript_scroll
                    .offset_from_tail
                    .saturating_add(PAGE_ROWS);
                self.dirty = true;
            }
            KeyCode::PageDown => {
                self.transcript_scroll.offset_from_tail = self
                    .transcript_scroll
                    .offset_from_tail
                    .saturating_sub(PAGE_ROWS);
                if self.transcript_scroll.offset_from_tail == 0 {
                    self.transcript_scroll.follow_tail = true;
                }
                self.dirty = true;
            }
            KeyCode::End => {
                self.transcript_scroll = TranscriptScroll::following_tail();
                self.dirty = true;
            }
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.input.input(Input {
                    key: Key::Enter,
                    ctrl: false,
                    alt: false,
                    shift: false,
                });
                self.pulse_input(16);
                self.dirty = true;
            }
            KeyCode::Enter => self.send_message().await?,
            _ if !self.voice_mode => {
                self.input.input(key);
                self.pulse_input(8);
                self.dirty = true;
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(key.code, KeyCode::Esc | KeyCode::F(1..=4)) {
            self.overlay = None;
            self.dirty = true;
            return Ok(());
        }

        let item_count = match self.overlay {
            Some(Overlay::Models) => self.models.len(),
            Some(Overlay::Sessions) => self.sessions.len(),
            Some(Overlay::Audio) => {
                self.audio_inventory.inputs.len() + self.audio_inventory.outputs.len()
            }
            Some(Overlay::Help | Overlay::Inspector) | None => 0,
        };
        match key.code {
            KeyCode::Up => {
                self.overlay_index = self.overlay_index.saturating_sub(1);
            }
            KeyCode::Down if item_count > 0 => {
                self.overlay_index = (self.overlay_index + 1).min(item_count - 1);
            }
            KeyCode::Enter => match self.overlay {
                Some(Overlay::Models) => self.select_model(self.overlay_index).await?,
                Some(Overlay::Sessions) => self.select_session(self.overlay_index).await?,
                Some(Overlay::Audio) => self.select_audio_device(self.overlay_index).await?,
                Some(Overlay::Help | Overlay::Inspector) | None => self.overlay = None,
            },
            KeyCode::Char('r') if self.overlay == Some(Overlay::Audio) => {
                self.refresh_audio_inventory();
            }
            _ => {}
        }
        self.dirty = true;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_internal_event(&mut self, event: InternalEvent) -> Result<()> {
        match event {
            InternalEvent::BackendReady { info, models } => {
                self.backend_refreshing = false;
                self.backend_info = Some(info);
                self.link = LinkState::Online;
                self.models = visible_models(models, self.config.allow_cloud_models);
                self.status = None;

                match choose_startup_model(
                    &self.models,
                    self.config.requested_model.as_deref(),
                    self.persisted.last_model.as_deref(),
                ) {
                    StartupModelChoice::Select(index) => self.select_model(index).await?,
                    StartupModelChoice::RequestedUnavailable => {
                        self.status = Some(
                            "REQUESTED MODEL UNAVAILABLE // select an installed local model"
                                .to_owned(),
                        );
                        self.open_overlay(Overlay::Models);
                    }
                    StartupModelChoice::NoModels => {
                        self.status = Some(
                            "NO LOCAL MODELS // load one in the configured chat backend".to_owned(),
                        );
                    }
                    StartupModelChoice::Picker if self.selected_model.is_none() => {
                        self.open_models();
                    }
                    StartupModelChoice::Picker => {}
                }
            }
            InternalEvent::BackendOffline(error) => {
                self.backend_refreshing = false;
                self.link = LinkState::Offline;
                self.status = Some(format!(
                    "{} OFFLINE // {} // Ctrl+R retry",
                    backend_label(self.config.backend),
                    error.message
                ));
            }
            InternalEvent::ModelHydrated(hydration) => {
                let ModelHydration {
                    name,
                    result,
                    running,
                } = *hydration;
                if self
                    .selected_model
                    .as_ref()
                    .is_none_or(|selected| selected.summary.name != name)
                {
                    return Ok(());
                }
                match result {
                    Ok(mut detailed) => {
                        if let Some(summary) = self.models.iter().find(|model| model.name == name) {
                            detailed.summary = summary.clone();
                        }
                        if let Ok(running) = running
                            && let Some(loaded) =
                                running.into_iter().find(|model| model.summary.name == name)
                        {
                            detailed.allocated_context = loaded.allocated_context;
                            detailed.loaded_size = loaded.loaded_size;
                            detailed.expires_at = loaded.expires_at;
                        }
                        self.selected_model = Some(detailed);
                    }
                    Err(error) => {
                        self.status = Some(format!("MODEL METADATA UNAVAILABLE // {error}"));
                    }
                }
            }
            InternalEvent::Chat(event) => self.handle_chat_event(event).await?,
            InternalEvent::SpeechProbe(result) => {
                self.speech_refreshing = false;
                match result {
                    Ok(_) => {
                        self.speech_online = true;
                        if self.voice_mode && self.voice_state == VoiceState::Disarmed {
                            self.voice_state = VoiceState::Ready;
                            self.status =
                                Some("QWEN VOICE LINK READY // Space to record".to_owned());
                        }
                    }
                    Err(error) => {
                        self.speech_online = false;
                        if self.voice_mode {
                            self.voice_state = VoiceState::Disarmed;
                            self.status =
                                Some(format!("MLX-AUDIO OFFLINE // {error} // run xikoi setup"));
                        }
                    }
                }
            }
            InternalEvent::Transcription(result) => {
                self.voice_cancel = None;
                match result {
                    Ok(text) => {
                        self.voice_state = VoiceState::Thinking;
                        self.status = Some(format!("VOICE DECODED // {text}"));
                        self.send_content(text, MessageOrigin::Speech).await?;
                    }
                    Err(error) => {
                        self.voice_state = VoiceState::Failed;
                        self.activity = ActivityState::Failed;
                        self.status = Some(format!("TRANSCRIPTION FAILED // {error}"));
                    }
                }
            }
            InternalEvent::TtsChunk(bytes) => {
                if let Some(playback) = self.playback.as_mut() {
                    playback.enqueue_pcm16(&bytes, QWEN_TTS_SAMPLE_RATE);
                    self.voice_state = VoiceState::Speaking;
                    self.activity = ActivityState::Streaming;
                    self.status = Some("AI VOICE // streaming local Qwen synthesis".to_owned());
                }
            }
            InternalEvent::TtsFinished(result) => match result {
                Ok(()) => {
                    self.voice_cancel = None;
                    self.tts_response_finished = true;
                }
                Err(error) => {
                    self.voice_cancel = None;
                    if let Some(playback) = self.playback.take() {
                        playback.stop();
                    }
                    self.tts_response_finished = false;
                    self.voice_state = VoiceState::Failed;
                    self.activity = ActivityState::Failed;
                    self.status = Some(format!("SPEECH SYNTHESIS FAILED // {error}"));
                }
            },
        }
        self.dirty = true;
        Ok(())
    }

    async fn handle_chat_event(&mut self, event: ChatEvent) -> Result<()> {
        let Some(index) = self.streaming_message else {
            return Ok(());
        };
        match event {
            ChatEvent::ContentDelta(delta) => {
                if let Some(message) = self.session.messages.get_mut(index) {
                    message.content.push_str(&delta);
                }
                self.activity = ActivityState::Streaming;
                if self.voice_mode {
                    self.voice_state = VoiceState::Thinking;
                }
                self.pulse_link((delta.chars().count() as u64).clamp(4, 80));
            }
            ChatEvent::ThinkingDelta(delta) => {
                self.activity = ActivityState::Thinking;
                if self.voice_mode {
                    self.voice_state = VoiceState::Thinking;
                }
                self.pulse_link((delta.chars().count() as u64).clamp(2, 36));
            }
            ChatEvent::Finished { reason, usage } => {
                let context_warning = context_warning(
                    &usage,
                    self.selected_model
                        .as_ref()
                        .and_then(|model| model.allocated_context),
                );
                if let Some(message) = self.session.messages.get_mut(index) {
                    message.status = MessageStatus::Complete;
                    message.finish_reason = reason;
                    message.usage = Some(usage.clone());
                }
                let spoken_response = self
                    .session
                    .messages
                    .get(index)
                    .map(|message| message.content.trim().to_owned())
                    .filter(|content| !content.is_empty());
                self.persist_streaming_message(index).await?;
                self.last_usage = Some(usage);
                self.finish_generation("LINK COMPLETE");
                if let Some(warning) = context_warning {
                    self.status = Some(warning);
                }
                self.refresh_selected_model();
                if self.voice_mode
                    && let Some(response) = spoken_response
                {
                    self.start_synthesis(&response);
                }
            }
            ChatEvent::Failed(error) => {
                if let Some(message) = self.session.messages.get_mut(index) {
                    message.status = MessageStatus::Failed;
                    message.error = Some(error.message.clone());
                }
                self.persist_streaming_message(index).await?;
                self.finish_generation("LINK FAILED");
                self.activity = ActivityState::Failed;
                if self.voice_mode {
                    self.voice_state = VoiceState::Failed;
                }
                self.status = Some(format!("GENERATION FAILED // {}", error.message));
            }
            ChatEvent::Cancelled => {
                if let Some(message) = self.session.messages.get_mut(index) {
                    message.status = MessageStatus::Cancelled;
                    message.finish_reason = Some("cancelled".to_owned());
                }
                self.persist_streaming_message(index).await?;
                self.finish_generation("LINK CANCELLED");
                if self.voice_mode && self.capture.is_none() {
                    self.voice_state = VoiceState::Ready;
                }
            }
        }
        Ok(())
    }

    fn handle_tick(&mut self) {
        if let Some(capture) = self.capture.as_mut() {
            capture.drain();
            let level = capture.level();
            let problem = capture.has_problem();
            push_signal(&mut self.input_signal_samples, level.max(1));
            if problem {
                self.status = Some(
                    "MICROPHONE BUFFER PRESSURE // capture remains bounded; stop and retry"
                        .to_owned(),
                );
            }
            self.dirty = true;
        }
        if let Some(playback) = self.playback.as_ref() {
            let level = playback.level();
            let problem = playback.has_problem();
            push_signal(&mut self.signal_samples, level.max(1));
            if problem {
                self.status = Some(
                    "OUTPUT BUFFER PRESSURE // some synthesized audio may have been dropped"
                        .to_owned(),
                );
            }
            self.dirty = true;
        }
        if self.tts_response_finished
            && self
                .playback
                .as_ref()
                .is_some_and(PlaybackSession::is_empty)
        {
            if let Some(playback) = self.playback.take() {
                playback.stop();
            }
            self.tts_response_finished = false;
            self.voice_state = VoiceState::Ready;
            self.activity = ActivityState::Idle;
            self.status = Some("VOICE LINK COMPLETE // Space to speak".to_owned());
            self.dirty = true;
        }
        if self.config.animation {
            if self.activity == ActivityState::Thinking
                && let Some(started) = self.generation_started
            {
                let phase = u64::try_from(started.elapsed().as_millis() / 80).unwrap_or(u64::MAX);
                push_signal(&mut self.signal_samples, 12 + (phase.wrapping_mul(17) % 52));
            }
            let link_active = decay_signal(&mut self.signal_samples);
            let input_active = decay_signal(&mut self.input_signal_samples);
            if link_active || input_active || self.generation_started.is_some() {
                self.dirty = true;
            }
        }
        if self.link == LinkState::Offline
            && !self.backend_refreshing
            && self.last_backend_attempt.elapsed() >= OFFLINE_RETRY
        {
            self.spawn_backend_refresh();
        }
        if self.config.speech.enabled
            && self.voice_mode
            && !self.speech_online
            && !self.speech_refreshing
            && self.last_speech_attempt.elapsed() >= OFFLINE_RETRY
        {
            self.spawn_speech_probe();
        }
    }

    async fn send_message(&mut self) -> Result<()> {
        let content = self.input.lines().join("\n");
        self.send_content(content.trim_end().to_owned(), MessageOrigin::Typed)
            .await
    }

    async fn send_content(&mut self, content: String, origin: MessageOrigin) -> Result<()> {
        if self.generation_cancel.is_some() {
            self.status = Some("GENERATION ACTIVE // Esc cancels the current link".to_owned());
            self.dirty = true;
            return Ok(());
        }
        let Some(model_name) = self
            .selected_model
            .as_ref()
            .map(|model| model.summary.name.clone())
        else {
            self.open_models();
            self.status = Some("SELECT A LOCAL MODEL BEFORE TRANSMITTING".to_owned());
            return Ok(());
        };
        if content.trim().is_empty() {
            return Ok(());
        }

        let mut user = ChatMessage::user(content);
        user.origin = origin;
        self.storage.append_message(self.session.id, &user).await?;
        self.session.messages.push(user);
        let request = ChatRequest {
            model: model_name.clone(),
            messages: self.session.messages.clone(),
            options: GenerationOptions {
                keep_alive: self.config.keep_alive.clone(),
            },
        };
        let assistant = ChatMessage::streaming_assistant(model_name);
        self.session.messages.push(assistant);
        self.streaming_message = Some(self.session.messages.len() - 1);
        self.input = TextArea::default();
        configure_input(&mut self.input);
        self.last_usage = None;
        self.generation_started = Some(Instant::now());
        self.activity = ActivityState::Thinking;
        if origin == MessageOrigin::Speech {
            self.voice_state = VoiceState::Thinking;
            self.status = Some("AI THINKING // local model link active".to_owned());
        } else {
            self.status = Some("ESTABLISHING MODEL LINK".to_owned());
        }
        self.transcript_scroll = TranscriptScroll::following_tail();

        let cancellation = CancellationToken::new();
        self.generation_cancel = Some(cancellation.clone());
        let backend = Arc::clone(&self.backend);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            match backend.stream_chat(request, cancellation).await {
                Ok(mut receiver) => {
                    while let Some(event) = receiver.recv().await {
                        if tx.send(InternalEvent::Chat(event)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(InternalEvent::Chat(ChatEvent::Failed(error))).await;
                }
            }
        });
        self.dirty = true;
        Ok(())
    }

    fn cancel_generation(&mut self) {
        if let Some(cancellation) = &self.generation_cancel {
            cancellation.cancel();
            self.activity = ActivityState::Cancelling;
            self.status = Some("CANCELLING MODEL LINK".to_owned());
            self.dirty = true;
        }
    }

    fn cancel_active_work(&mut self) {
        self.cancel_voice();
        self.cancel_generation();
    }

    fn cancel_voice(&mut self) {
        if let Some(cancellation) = self.voice_cancel.take() {
            cancellation.cancel();
        }
        if let Some(capture) = self.capture.take() {
            drop(capture.finish());
        }
        if let Some(playback) = self.playback.take() {
            playback.stop();
        }
        self.tts_response_finished = false;
        self.voice_state = if self.voice_mode && self.speech_online {
            VoiceState::Ready
        } else {
            VoiceState::Disarmed
        };
        self.dirty = true;
    }

    async fn request_quit(&mut self) -> Result<()> {
        self.cancel_voice();
        if let Some(cancellation) = self.generation_cancel.take() {
            cancellation.cancel();
        }
        if let Some(index) = self.streaming_message.take() {
            if let Some(message) = self.session.messages.get_mut(index) {
                message.status = MessageStatus::Cancelled;
                message.finish_reason = Some("shutdown".to_owned());
            }
            self.persist_streaming_message(index).await?;
        }
        self.persisted.recent_session = Some(self.session.id.to_string());
        self.storage.save_state(&self.persisted).await?;
        self.should_quit = true;
        Ok(())
    }

    fn finish_generation(&mut self, status: &str) {
        self.streaming_message = None;
        self.generation_cancel = None;
        self.generation_started = None;
        if self.activity != ActivityState::Failed {
            self.activity = ActivityState::Idle;
            self.status = Some(status.to_owned());
        }
    }

    async fn persist_streaming_message(&self, index: usize) -> Result<()> {
        if let Some(message) = self.session.messages.get(index) {
            self.storage
                .append_message(self.session.id, message)
                .await?;
        }
        Ok(())
    }

    fn spawn_backend_refresh(&mut self) {
        if self.backend_refreshing {
            return;
        }
        self.backend_refreshing = true;
        self.last_backend_attempt = Instant::now();
        self.link = LinkState::Connecting;
        self.status = Some(format!(
            "PROBING {} LINK",
            backend_label(self.config.backend)
        ));
        self.dirty = true;

        let backend = Arc::clone(&self.backend);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            match backend.probe().await {
                Ok(info) => match backend.list_models().await {
                    Ok(models) => {
                        let _ = tx.send(InternalEvent::BackendReady { info, models }).await;
                    }
                    Err(error) => {
                        let _ = tx.send(InternalEvent::BackendOffline(error)).await;
                    }
                },
                Err(error) => {
                    let _ = tx.send(InternalEvent::BackendOffline(error)).await;
                }
            }
        });
    }

    fn spawn_speech_probe(&mut self) {
        if !self.config.speech.enabled || self.speech_refreshing {
            return;
        }
        self.speech_refreshing = true;
        self.last_speech_attempt = Instant::now();
        if self.voice_mode {
            self.status = Some("PROBING LOCAL MLX-AUDIO LINK".to_owned());
        }
        let speech = Arc::clone(&self.speech);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = speech.probe().await.map_err(|error| error.to_string());
            let _ = tx.send(InternalEvent::SpeechProbe(result)).await;
        });
    }

    async fn toggle_voice_mode(&mut self) -> Result<()> {
        if self.voice_mode {
            self.voice_mode = false;
            self.cancel_voice();
            self.voice_state = VoiceState::Disarmed;
            self.status = Some("TYPE INTERFACE ACTIVE // F3 restores voice deck".to_owned());
        } else if !self.config.speech.enabled {
            self.status =
                Some("VOICE LAYER DISABLED // remove --no-speech or enable config".to_owned());
            return Ok(());
        } else {
            self.voice_mode = true;
            self.voice_state = if self.speech_online {
                VoiceState::Ready
            } else {
                VoiceState::Disarmed
            };
            if self.speech_online {
                self.status = Some("VOICE INTERFACE ACTIVE // Space to record".to_owned());
            } else {
                self.status = Some("VOICE INTERFACE WAITING // run xikoi setup".to_owned());
                self.spawn_speech_probe();
            }
        }
        self.persisted.voice_mode = self.voice_mode;
        self.storage.save_state(&self.persisted).await?;
        self.dirty = true;
        Ok(())
    }

    async fn handle_voice_trigger(&mut self) -> Result<()> {
        match self.voice_state {
            VoiceState::Listening => {
                self.stop_recording();
                Ok(())
            }
            VoiceState::Ready | VoiceState::Failed => self.start_recording().await,
            VoiceState::Speaking | VoiceState::Synthesizing => {
                self.cancel_voice();
                self.start_recording().await
            }
            VoiceState::Thinking => {
                self.cancel_generation();
                self.start_recording().await
            }
            VoiceState::Transcribing => {
                self.status = Some("TRANSCRIPTION ACTIVE // Esc cancels".to_owned());
                self.dirty = true;
                Ok(())
            }
            VoiceState::Disarmed => {
                self.status = Some("MLX-AUDIO OFFLINE // run xikoi setup, then Ctrl+R".to_owned());
                self.spawn_speech_probe();
                self.dirty = true;
                Ok(())
            }
        }
    }

    async fn start_recording(&mut self) -> Result<()> {
        if !self.speech_online {
            self.voice_state = VoiceState::Disarmed;
            self.status = Some("MLX-AUDIO OFFLINE // voice capture is disarmed".to_owned());
            self.spawn_speech_probe();
            return Ok(());
        }
        if self.selected_model.is_none() {
            self.open_models();
            self.status = Some("SELECT A LOCAL CHAT MODEL BEFORE RECORDING".to_owned());
            return Ok(());
        }
        self.cancel_voice();
        match CaptureSession::start(self.persisted.input_device_id.as_deref()) {
            Ok(capture) => {
                self.persisted.input_device_id = Some(capture.device_id.clone());
                self.storage.save_state(&self.persisted).await?;
                self.status = Some(format!(
                    "MIC LIVE // {} // Space stops and transcribes",
                    capture.device_name
                ));
                self.capture = Some(capture);
                self.voice_state = VoiceState::Listening;
                self.activity = ActivityState::Idle;
            }
            Err(error) => {
                self.voice_state = VoiceState::Failed;
                self.activity = ActivityState::Failed;
                self.status = Some(format!("MICROPHONE UNAVAILABLE // {error}"));
            }
        }
        self.dirty = true;
        Ok(())
    }

    fn stop_recording(&mut self) {
        let Some(capture) = self.capture.take() else {
            return;
        };
        let audio = capture.finish();
        if audio.duration_seconds() < 0.25 {
            self.voice_state = VoiceState::Ready;
            self.status =
                Some("VOICE CAPTURE TOO SHORT // hold Space between start and stop".to_owned());
            return;
        }
        let wav = match audio.to_asr_wav() {
            Ok(wav) => wav,
            Err(error) => {
                self.voice_state = VoiceState::Failed;
                self.activity = ActivityState::Failed;
                self.status = Some(format!("AUDIO ENCODE FAILED // {error}"));
                return;
            }
        };
        let request = TranscriptionRequest {
            model: self.config.speech.asr_model.clone(),
            wav,
            language: None,
        };
        let cancellation = CancellationToken::new();
        self.voice_cancel = Some(cancellation.clone());
        self.voice_state = VoiceState::Transcribing;
        self.status = Some(format!(
            "QWEN ASR // decoding {:.1}s locally",
            audio.duration_seconds()
        ));
        let speech = Arc::clone(&self.speech);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = speech.transcribe(request, &cancellation).await;
            if matches!(result, Err(SpeechFailure::Cancelled)) {
                return;
            }
            let result = result.map_err(|error| error.to_string());
            let _ = tx.send(InternalEvent::Transcription(result)).await;
        });
        self.dirty = true;
    }

    fn start_synthesis(&mut self, content: &str) {
        let spoken = text_for_speech(content);
        if spoken.is_empty() {
            self.voice_state = VoiceState::Ready;
            return;
        }
        let playback = match PlaybackSession::start(self.persisted.output_device_id.as_deref()) {
            Ok(playback) => playback,
            Err(error) => {
                self.voice_state = VoiceState::Failed;
                self.activity = ActivityState::Failed;
                self.status = Some(format!("AUDIO OUTPUT UNAVAILABLE // {error}"));
                return;
            }
        };
        self.status = Some(format!(
            "QWEN TTS // {} // {}",
            self.config.speech.tts_voice, playback.device_name
        ));
        self.playback = Some(playback);
        self.tts_response_finished = false;
        self.voice_state = VoiceState::Synthesizing;
        self.activity = ActivityState::Thinking;

        let request = SynthesisRequest::qwen(
            self.config.speech.tts_model.clone(),
            spoken,
            self.config.speech.tts_voice.clone(),
        );
        let cancellation = CancellationToken::new();
        self.voice_cancel = Some(cancellation.clone());
        let speech = Arc::clone(&self.speech);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let response = match speech.synthesize(&request, &cancellation).await {
                Ok(response) => response,
                Err(SpeechFailure::Cancelled) => return,
                Err(error) => {
                    let _ = tx
                        .send(InternalEvent::TtsFinished(Err(error.to_string())))
                        .await;
                    return;
                }
            };
            let mut stream = response.bytes_stream();
            loop {
                let item = tokio::select! {
                    () = cancellation.cancelled() => return,
                    item = stream.next() => item,
                };
                match item {
                    Some(Ok(bytes)) => {
                        if tx
                            .send(InternalEvent::TtsChunk(bytes.to_vec()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        let _ = tx
                            .send(InternalEvent::TtsFinished(Err(error.to_string())))
                            .await;
                        return;
                    }
                    None => {
                        let _ = tx.send(InternalEvent::TtsFinished(Ok(()))).await;
                        return;
                    }
                }
            }
        });
    }

    fn refresh_audio_inventory(&mut self) {
        match AudioInventory::discover() {
            Ok(inventory) => {
                self.audio_inventory = inventory;
                self.overlay_index = self.overlay_index.min(
                    self.audio_inventory
                        .inputs
                        .len()
                        .saturating_add(self.audio_inventory.outputs.len())
                        .saturating_sub(1),
                );
                self.status = Some("AUDIO MATRIX REFRESHED".to_owned());
            }
            Err(error) => {
                self.status = Some(format!("AUDIO ENUMERATION FAILED // {error}"));
            }
        }
        self.dirty = true;
    }

    fn open_audio(&mut self) {
        self.refresh_audio_inventory();
        self.open_overlay(Overlay::Audio);
    }

    async fn select_audio_device(&mut self, index: usize) -> Result<()> {
        let device = self
            .audio_inventory
            .inputs
            .iter()
            .chain(self.audio_inventory.outputs.iter())
            .nth(index)
            .cloned();
        let Some(device) = device else {
            return Ok(());
        };
        match device.direction {
            AudioDirection::Input => self.persisted.input_device_id = Some(device.id.clone()),
            AudioDirection::Output => self.persisted.output_device_id = Some(device.id.clone()),
        }
        self.storage.save_state(&self.persisted).await?;
        self.status = Some(format!(
            "{} ROUTED // {}",
            match device.direction {
                AudioDirection::Input => "MIC",
                AudioDirection::Output => "OUTPUT",
            },
            device.name
        ));
        self.overlay = None;
        Ok(())
    }

    fn refresh_selected_model(&self) {
        let Some(name) = self
            .selected_model
            .as_ref()
            .map(|model| model.summary.name.clone())
        else {
            return;
        };
        let backend = Arc::clone(&self.backend);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let (result, running) =
                tokio::join!(backend.show_model(&name), backend.running_models());
            let _ = tx
                .send(InternalEvent::ModelHydrated(Box::new(ModelHydration {
                    name,
                    result,
                    running,
                })))
                .await;
        });
    }

    async fn select_model(&mut self, index: usize) -> Result<()> {
        if self.generation_cancel.is_some() {
            self.status = Some("CANCEL GENERATION BEFORE SWITCHING MODELS".to_owned());
            self.overlay = None;
            return Ok(());
        }
        let Some(summary) = self.models.get(index).cloned() else {
            return Ok(());
        };
        if summary.is_cloud() && !self.config.allow_cloud_models {
            self.status = Some("CLOUD MODEL BLOCKED // enable explicitly in config".to_owned());
            return Ok(());
        }
        self.selected_model = Some(SelectedModel {
            summary: summary.clone(),
            ..SelectedModel::default()
        });
        self.persisted.last_model = Some(summary.name.clone());
        self.storage.save_state(&self.persisted).await?;
        self.overlay = None;
        self.status = Some(format!("MODEL SELECTED // {}", summary.name));
        self.refresh_selected_model();
        Ok(())
    }

    async fn select_session(&mut self, index: usize) -> Result<()> {
        if self.generation_cancel.is_some() {
            self.status = Some("CANCEL GENERATION BEFORE SWITCHING SESSIONS".to_owned());
            self.overlay = None;
            return Ok(());
        }
        let Some(summary) = self.sessions.get(index) else {
            return Ok(());
        };
        let session = self.storage.load_session(summary.id).await?;
        self.session = session;
        self.persisted.recent_session = Some(self.session.id.to_string());
        self.storage.save_state(&self.persisted).await?;
        self.last_usage = self
            .session
            .messages
            .iter()
            .rev()
            .find_map(|message| message.usage.clone());
        self.transcript_scroll = TranscriptScroll::following_tail();
        self.overlay = None;
        self.status = Some("SESSION LINK RESTORED".to_owned());
        Ok(())
    }

    async fn new_session(&mut self) -> Result<()> {
        if self.generation_cancel.is_some() {
            self.status = Some("CANCEL GENERATION BEFORE OPENING A NEW LINK".to_owned());
            self.dirty = true;
            return Ok(());
        }
        self.session = self.storage.create_session().await?;
        self.persisted.recent_session = Some(self.session.id.to_string());
        self.storage.save_state(&self.persisted).await?;
        self.sessions = self.storage.list_sessions().await?;
        self.last_usage = None;
        self.transcript_scroll = TranscriptScroll::following_tail();
        self.status = Some("NEW SESSION LINK".to_owned());
        self.overlay = None;
        self.dirty = true;
        Ok(())
    }

    fn open_models(&mut self) {
        self.open_overlay(Overlay::Models);
        if self.models.is_empty() {
            self.status = Some("NO LOCAL MODELS DETECTED".to_owned());
        }
    }

    async fn open_sessions(&mut self) -> Result<()> {
        self.sessions = self.storage.list_sessions().await?;
        self.open_overlay(Overlay::Sessions);
        Ok(())
    }

    fn open_overlay(&mut self, overlay: Overlay) {
        self.overlay = Some(overlay);
        self.overlay_index = 0;
        self.dirty = true;
    }

    fn pulse_link(&mut self, value: u64) {
        push_signal(&mut self.signal_samples, value);
    }

    fn pulse_input(&mut self, value: u64) {
        push_signal(&mut self.input_signal_samples, value);
    }
}

fn configure_input(input: &mut TextArea<'static>) {
    input.set_placeholder_text("Transmit a message to the selected local model…");
}

const fn backend_label(backend: BackendKind) -> &'static str {
    match backend {
        BackendKind::Ollama => "OLLAMA",
        BackendKind::LlamaCpp => "LLAMA.CPP",
    }
}

fn selected_device_name<'a>(devices: &'a [AudioDevice], selected_id: Option<&str>) -> &'a str {
    selected_id
        .and_then(|id| devices.iter().find(|device| device.id == id))
        .or_else(|| devices.iter().find(|device| device.is_default))
        .or_else(|| devices.first())
        .map_or("SYSTEM DEFAULT", |device| device.name.as_str())
}

fn text_for_speech(content: &str) -> String {
    let mut spoken = String::with_capacity(content.len());
    let mut previous_space = false;
    for character in content.chars() {
        let character = match character {
            '#' | '*' | '_' | '`' | '[' | ']' => ' ',
            other => other,
        };
        if character.is_whitespace() {
            if !previous_space {
                spoken.push(' ');
                previous_space = true;
            }
        } else {
            spoken.push(character);
            previous_space = false;
        }
    }
    spoken.trim().to_owned()
}

fn choose_startup_model(
    models: &[ModelSummary],
    requested: Option<&str>,
    last_used: Option<&str>,
) -> StartupModelChoice {
    if let Some(requested) = requested {
        return models
            .iter()
            .position(|model| model.name == requested)
            .map_or(
                StartupModelChoice::RequestedUnavailable,
                StartupModelChoice::Select,
            );
    }
    if let Some(index) =
        last_used.and_then(|name| models.iter().position(|model| model.name == name))
    {
        return StartupModelChoice::Select(index);
    }
    match models.len() {
        0 => StartupModelChoice::NoModels,
        1 => StartupModelChoice::Select(0),
        _ => StartupModelChoice::Picker,
    }
}

fn visible_models(models: Vec<ModelSummary>, allow_cloud_models: bool) -> Vec<ModelSummary> {
    models
        .into_iter()
        .filter(|model| allow_cloud_models || !model.is_cloud())
        .collect()
}

fn context_warning(usage: &Usage, allocated_context: Option<u64>) -> Option<String> {
    let ratio = usage.context_ratio(allocated_context)?;
    (ratio >= CONTEXT_WARNING_RATIO).then(|| {
        format!(
            "CONTEXT WARNING // {:.0}% of allocated context used",
            ratio * 100.0
        )
    })
}

fn push_signal(samples: &mut VecDeque<u64>, value: u64) {
    if samples.len() >= SIGNAL_CAPACITY {
        samples.pop_front();
    }
    samples.push_back(value);
}

fn decay_signal(samples: &mut VecDeque<u64>) -> bool {
    let previous = samples.back().copied().unwrap_or_default();
    let next = previous.saturating_mul(3) / 4;
    push_signal(samples, next);
    next > 0
}

fn model_detail(model: &SelectedModel) -> Option<String> {
    let mut details = Vec::new();
    if let Some(parameter_size) = &model.summary.details.parameter_size {
        details.push(parameter_size.clone());
    }
    if let Some(quantization) = &model.summary.details.quantization_level {
        details.push(quantization.clone());
    }
    if let Some(size) = model.loaded_size {
        details.push(format_bytes(size));
    }
    (!details.is_empty()).then(|| details.join(" · "))
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        let whole = bytes / GIB;
        let decimal = bytes.saturating_sub(whole * GIB).saturating_mul(10) / GIB;
        format!("{whole}.{decimal} GiB")
    } else {
        format!("{} MiB", bytes / MIB)
    }
}

fn short_id(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const fn nanos_to_millis(nanos: u64) -> u64 {
    nanos / 1_000_000
}

fn help_bindings() -> Vec<HelpBinding<'static>> {
    vec![
        HelpBinding {
            keys: "Enter",
            action: "send message",
        },
        HelpBinding {
            keys: "Alt+Enter / Ctrl+N",
            action: "insert newline",
        },
        HelpBinding {
            keys: "Esc",
            action: "cancel voice, playback, generation, or overlay",
        },
        HelpBinding {
            keys: "F3",
            action: "toggle typing and neural voice modes",
        },
        HelpBinding {
            keys: "F4",
            action: "select microphone and output route",
        },
        HelpBinding {
            keys: "Space (voice)",
            action: "start/stop capture or manually barge in",
        },
        HelpBinding {
            keys: "PgUp / PgDn / End",
            action: "navigate transcript",
        },
        HelpBinding {
            keys: "Ctrl+K",
            action: "select local model",
        },
        HelpBinding {
            keys: "Ctrl+O",
            action: "open saved session",
        },
        HelpBinding {
            keys: "Ctrl+L",
            action: "start new session",
        },
        HelpBinding {
            keys: "Ctrl+R",
            action: "retry Ollama and MLX-Audio links",
        },
        HelpBinding {
            keys: "F2",
            action: "open telemetry inspector",
        },
        HelpBinding {
            keys: "Ctrl+C",
            action: "save and quit",
        },
    ]
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        backend::ChatEventReceiver,
        config::{AppPaths, ColorMode},
    };

    struct MockBackend;

    #[async_trait]
    impl ChatBackend for MockBackend {
        async fn probe(&self) -> Result<BackendInfo, BackendFailure> {
            Ok(BackendInfo {
                version: "test".to_owned(),
                endpoint: "http://127.0.0.1:11434".to_owned(),
            })
        }

        async fn list_models(&self) -> Result<Vec<ModelSummary>, BackendFailure> {
            Ok(Vec::new())
        }

        async fn running_models(&self) -> Result<Vec<SelectedModel>, BackendFailure> {
            Ok(Vec::new())
        }

        async fn show_model(&self, model: &str) -> Result<SelectedModel, BackendFailure> {
            Ok(SelectedModel {
                summary: ModelSummary {
                    name: model.to_owned(),
                    ..ModelSummary::default()
                },
                ..SelectedModel::default()
            })
        }

        async fn stream_chat(
            &self,
            _request: ChatRequest,
            _cancellation: CancellationToken,
        ) -> Result<ChatEventReceiver, BackendFailure> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }

    async fn test_app() -> (TempDir, App) {
        let temporary = TempDir::new().unwrap();
        let paths = AppPaths::from_root(temporary.path().join("xikoi"));
        let storage = Storage::new(paths.clone());
        storage.initialize().await.unwrap();
        let config = RuntimeConfig {
            backend: crate::config::BackendKind::Ollama,
            ollama_url: "http://127.0.0.1:11434".parse().unwrap(),
            llama_url: "http://127.0.0.1:8080".parse().unwrap(),
            requested_model: None,
            resume: None,
            keep_alive: None,
            color: ColorMode::Mono,
            animation: false,
            allow_cloud_models: false,
            speech: crate::config::SpeechConfig {
                enabled: false,
                url: "http://127.0.0.1:8000".parse().unwrap(),
                asr_model: "qwen-asr".to_owned(),
                tts_model: "qwen-tts".to_owned(),
                tts_voice: "Vivian".to_owned(),
            },
            paths,
        };
        let app = App::bootstrap(config, storage, Arc::new(MockBackend))
            .await
            .unwrap();
        (temporary, app)
    }

    #[test]
    fn signals_are_bounded_and_decay() {
        let mut samples = VecDeque::new();
        for value in 0..(SIGNAL_CAPACITY as u64 + 20) {
            push_signal(&mut samples, value);
        }
        assert_eq!(samples.len(), SIGNAL_CAPACITY);
        let last = samples.back().copied().unwrap();
        assert!(decay_signal(&mut samples));
        assert_eq!(samples.back().copied(), Some(last * 3 / 4));
    }

    #[test]
    fn formats_loaded_model_size() {
        assert_eq!(format_bytes(6 * 1024 * 1024 * 1024), "6.0 GiB");
        assert_eq!(format_bytes(512 * 1024 * 1024), "512 MiB");
    }

    #[test]
    fn explicit_model_never_falls_through_to_a_different_model() {
        let models = vec![ModelSummary {
            name: "installed".to_owned(),
            ..ModelSummary::default()
        }];
        assert_eq!(
            choose_startup_model(&models, Some("missing"), Some("installed")),
            StartupModelChoice::RequestedUnavailable
        );
    }

    #[test]
    fn startup_model_uses_last_then_single_then_picker() {
        let models = vec![
            ModelSummary {
                name: "first".to_owned(),
                ..ModelSummary::default()
            },
            ModelSummary {
                name: "last".to_owned(),
                ..ModelSummary::default()
            },
        ];
        assert_eq!(
            choose_startup_model(&models, None, Some("last")),
            StartupModelChoice::Select(1)
        );
        assert_eq!(
            choose_startup_model(&models[..1], None, None),
            StartupModelChoice::Select(0)
        );
        assert_eq!(
            choose_startup_model(&models, None, None),
            StartupModelChoice::Picker
        );
    }

    #[test]
    fn warns_at_eighty_percent_of_confirmed_allocation() {
        let usage = Usage {
            prompt_eval_count: Some(8_000),
            ..Usage::default()
        };
        assert!(context_warning(&usage, Some(10_000)).is_some());
        assert!(context_warning(&usage, Some(10_001)).is_none());
        assert!(context_warning(&usage, None).is_none());
    }

    #[test]
    fn cloud_models_are_hidden_without_explicit_opt_in() {
        let local = ModelSummary {
            name: "qwen3.5:9b".to_owned(),
            ..ModelSummary::default()
        };
        let cloud = ModelSummary {
            name: "gpt-oss:20b-cloud".to_owned(),
            remote_model: Some("gpt-oss:20b".to_owned()),
            remote_host: Some("https://ollama.com".to_owned()),
            ..ModelSummary::default()
        };

        assert_eq!(
            visible_models(vec![local.clone(), cloud.clone()], false),
            vec![local.clone()]
        );
        assert_eq!(
            visible_models(vec![local.clone(), cloud.clone()], true),
            vec![local, cloud]
        );
    }

    #[test]
    fn composer_accepts_unicode_and_multiline_input() {
        let mut input = TextArea::default();
        for character in ['λ', '界', '🧠'] {
            input.input(Input {
                key: Key::Char(character),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }
        input.input(Input {
            key: Key::Enter,
            ctrl: false,
            alt: false,
            shift: false,
        });
        input.insert_str("naïve");

        assert_eq!(input.lines(), ["λ界🧠", "naïve"]);
    }

    #[test]
    fn speech_cleanup_removes_markdown_control_noise() {
        assert_eq!(
            text_for_speech("# **Hello**\n`local code`"),
            "Hello local code"
        );
    }

    #[tokio::test]
    async fn transcribed_turns_keep_voice_provenance() {
        let (_temporary, mut app) = test_app().await;
        app.selected_model = Some(SelectedModel {
            summary: ModelSummary {
                name: "qwen".to_owned(),
                ..ModelSummary::default()
            },
            ..SelectedModel::default()
        });
        app.send_content("spoken locally".to_owned(), MessageOrigin::Speech)
            .await
            .unwrap();

        assert_eq!(app.session.messages[0].origin, MessageOrigin::Speech);
        assert_eq!(app.session.messages[0].content, "spoken locally");
    }

    #[tokio::test]
    async fn backend_ready_filters_cloud_and_selects_the_only_local_model() {
        let (_temporary, mut app) = test_app().await;
        let local = ModelSummary {
            name: "qwen3.5:9b".to_owned(),
            ..ModelSummary::default()
        };
        let cloud = ModelSummary {
            name: "gpt-oss:20b-cloud".to_owned(),
            remote_host: Some("https://ollama.com".to_owned()),
            ..ModelSummary::default()
        };
        app.handle_internal_event(InternalEvent::BackendReady {
            info: BackendInfo {
                version: "test".to_owned(),
                endpoint: "http://127.0.0.1:11434".to_owned(),
            },
            models: vec![local.clone(), cloud],
        })
        .await
        .unwrap();

        assert_eq!(app.models, vec![local]);
        assert_eq!(
            app.selected_model
                .as_ref()
                .map(|model| model.summary.name.as_str()),
            Some("qwen3.5:9b")
        );
        assert_eq!(app.persisted.last_model.as_deref(), Some("qwen3.5:9b"));
    }

    #[tokio::test]
    async fn shutdown_persists_partial_output_before_quitting() {
        let (_temporary, mut app) = test_app().await;
        let mut assistant = ChatMessage::streaming_assistant("qwen3.5:9b");
        assistant.content = "partial but valuable".to_owned();
        app.session.messages.push(assistant);
        app.streaming_message = Some(0);
        app.generation_cancel = Some(CancellationToken::new());

        app.request_quit().await.unwrap();

        assert!(app.should_quit);
        let loaded = app.storage.load_session(app.session.id).await.unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "partial but valuable");
        assert_eq!(loaded.messages[0].status, MessageStatus::Cancelled);
        assert_eq!(
            loaded.messages[0].finish_reason.as_deref(),
            Some("shutdown")
        );
    }
}
