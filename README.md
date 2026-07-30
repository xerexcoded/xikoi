# Xikoi

![](xikoi.png)

Xikoi is a local-first terminal interface for talking to models served by
[Ollama](https://ollama.com). It is written in Rust with
[Ratatui](https://ratatui.rs) and designed first for Apple Silicon Macs.

The visual thesis is a neuromancer control deck rather than a conventional
chat window: the conversation is the primary signal field, the composer is a
compact command surface, and live model telemetry stays visible beside it.
Phosphor green carries the signal over near-black panels, pale text remains
readable, and amber or crimson is reserved for warning and failure states.
Motion is restrained to meaningful activity and can be disabled. The first
milestone is keyboard chat. Signal visualizers do not capture microphone audio.

## Status

Xikoi is an early-stage application. The current contract is:

- streamed local chat through Ollama's HTTP API;
- selection from models already installed in Ollama;
- exact token and timing metrics from Ollama's final stream record;
- local JSONL session history and file-based diagnostics;
- responsive wide, compact, minimal, and safe too-small terminal states;
- no automatic model downloads, microphone access, analytics, or telemetry.

## Prerequisites

1. Install [Nix](https://nixos.org/download/) and
   [direnv](https://direnv.net/).
2. On macOS, install and launch the native Ollama application. Its Apple
   Silicon build uses Metal acceleration.
3. Install at least one model yourself:

   ```sh
   ollama pull qwen3:1.7b
   ollama list
   ```

Xikoi never pulls a model automatically and does not manage the Ollama server
lifecycle. If Ollama is unavailable, the interface remains open with a retryable
offline state.

## Development environment

The repository pins the current `nixpkgs-unstable` revision and supports
`aarch64-darwin`, `x86_64-darwin`, `aarch64-linux`, and `x86_64-linux`.
Because Nixpkgs unstable 26.11 dropped Intel macOS, `x86_64-darwin` alone uses
the pinned, security-maintained `nixpkgs-26.05-darwin` branch.
The committed `flake.lock` locks the complete Nix toolchain and `Cargo.lock`
locks Rust dependencies. Rust, Cargo, rustfmt, Clippy, rust-analyzer, test
tools, and Nix tooling all come from the flake; a separate rustup toolchain is
neither needed nor used. A pinned `rust-overlay` input supplies the official
stable Rust toolchain, currently Rust and Cargo 1.97.1, and the Nix package
build uses that exact same compiler and Cargo.

Allow direnv once:

```sh
direnv allow
```

The installed direnv `use flake` helper enables Nix's flake commands for the
project. To enter manually on a machine where experimental features are not
enabled globally, use:

```sh
nix --extra-experimental-features 'nix-command flakes' develop
```

Zed integration is configured in `.zed/settings.json`. It asks Zed to load
the project's direnv shell and use the flake-provided `rust-analyzer` and
`nixd` binaries. Restart the language servers from Zed's command palette after
first allowing direnv.

Start Xikoi with the model picker:

```sh
cargo run
```

Or request an installed model explicitly:

```sh
cargo run -- --model qwen3:1.7b
```

The Ollama endpoint defaults to `http://127.0.0.1:11434`. Override it with
`--ollama-url`, `XIKOI_OLLAMA_URL`, or Ollama's `OLLAMA_HOST`.

## Keymap

| Key | Action |
| --- | --- |
| `Enter` | Send the composer contents |
| `Alt+Enter` or `Ctrl+N` | Insert a newline |
| `Esc` | Cancel generation or close the active overlay |
| `PageUp` / `PageDown` | Scroll the conversation |
| `End` | Re-pin the viewport to the newest message |
| `Ctrl+K` | Open the model picker |
| `Ctrl+O` | Open saved sessions |
| `Ctrl+L` | Start a new session |
| `Ctrl+R` | Retry the Ollama connection |
| `F1` | Toggle help |
| `F2` | Open the full telemetry inspector |
| `Ctrl+C` | Persist any partial response, then quit |

The transcript auto-follows while pinned. Scrolling upward unpins it; `End`
resumes live following.

## Configuration and local storage

Configuration precedence for the Ollama endpoint is:
`--ollama-url`, `XIKOI_OLLAMA_URL`, `OLLAMA_HOST`, `config.toml`, then the
localhost default.

Other environment overrides are `XIKOI_MODEL`, `XIKOI_RESUME`,
`XIKOI_KEEP_ALIVE`, `XIKOI_COLOR`, `XIKOI_NO_ANIMATION`, and
`XIKOI_ALLOW_CLOUD_MODELS`. Command-line values take priority, followed by
Xikoi environment variables and then `config.toml`.

Xikoi follows `XDG_CONFIG_HOME` when it is set. Otherwise all application data
lives under `~/.config/xikoi`:

| Path | Purpose |
| --- | --- |
| `config.toml` | User configuration |
| `state.json` | Last model and recent-session pointer |
| `chats/` | One append-safe JSONL file per session |
| `logs/` | Private per-run diagnostic logs; newest ten retained |

Example `~/.config/xikoi/config.toml`:

```toml
ollama_url = "http://127.0.0.1:11434"
keep_alive = "5m"
color = "auto"
animation = true
allow_cloud_models = false
```

Useful command-line overrides:

```sh
cargo run -- --color ansi --no-animation
cargo run -- --resume <session-uuid>
```

Valid color modes are `auto`, `truecolor`, `ansi`, and `mono`.

## Privacy boundary

With the default localhost endpoint and a local model, prompts and responses
stay between Xikoi and the Ollama process on the same machine. Xikoi sends no
analytics or usage telemetry.

Sessions are saved as plaintext JSONL under `chats/`; anyone with access to
those files can read them. Diagnostic logs can contain model names, timings,
and error details, but should not contain message bodies.

Cloud-backed models are refused unless `allow_cloud_models = true` is set
explicitly. Pointing `ollama_url` at a remote host or enabling cloud models
means prompts leave the device and are governed by that service's policies.
The interface must visibly distinguish that state from local inference.

## Test and build

Run the standard quality gate inside the activated Nix environment:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features
cargo audit
cargo build --release
nix flake check
```

Build or run the flake package:

```sh
nix build
nix run . -- --model qwen3:1.7b
```

If flakes are not enabled globally, the same fallback applies:

```sh
nix --extra-experimental-features 'nix-command flakes' flake check
nix --extra-experimental-features 'nix-command flakes' build
```

Normal tests use mocked HTTP fixtures and never require Ollama. The live smoke
test is ignored by default; opt in only after starting Ollama and choosing an
already-installed model:

```sh
OLLAMA_INTEGRATION_MODEL=qwen3:1.7b \
  cargo test --test ollama_smoke -- --ignored --nocapture
```

CI runs the Nix-defined checks on both `x86_64-linux` and native
`aarch64-darwin`. It does not start Ollama or download model weights.
