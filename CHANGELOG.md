# Changelog

All notable Xikoi changes are recorded here. Releases follow semantic
versioning.

## 0.1.1 - 2026-08-13

- Render dual scope-tui oscilloscopes for live microphone and synthesized-speaker PCM.
- Show the AI oscilloscope in the top visualizer and the user oscilloscope only in the voice deck.
- Ignore stale transcription and TTS events after a newer voice operation starts.
- Strip terminal control sequences from model output, errors, and status text.
- Bound non-streaming HTTP bodies and add read deadlines for health and metadata calls.
- Reject oversized llama.cpp SSE records, including newline-terminated and final lines.
- Bind the managed MLX-Audio server to the configured loopback address, including IPv6.
- Keep linear resampling state across streamed PCM chunks.

## 0.1.0 - 2026-08-03

First public release.

- Added the responsive Neuromancer-inspired Ratatui chat interface.
- Added streamed local chat through Ollama and llama.cpp.
- Added private JSONL sessions, diagnostics, model selection, and telemetry.
- Added native microphone capture, audio-device routing, Qwen3-ASR, and
  streaming Qwen3-TTS on Apple Silicon.
- Added guided `xikoi setup`, dependency diagnostics through `xikoi doctor`,
  eager model downloads, real voice warm-up, and managed MLX-Audio lifecycle.
- Added crates.io, GitHub release, and Homebrew tap packaging.
