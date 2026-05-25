# gamechat

Voice-driven supervisor for [Claude Code] and [Codex]. You talk to a low-latency Realtime model in your terminal; whenever you ask for real work, it dispatches a job to a background coding agent and narrates the result when it lands.

```
🎤 ──▶ OpenAI Realtime (gpt-realtime-2) ──▶ 🔈
                │
                │  tool: delegate_to_orchestrator(slug, intent, …)
                │  tool: sub_agent_progress(slug)
                ▼
        OrchestratorJobManager
                │  one worker task per slug, ordered within a slug,
                │  concurrent across slugs
                ▼
        ┌───────┴────────┐
   `claude -p`        `codex exec`
   (Claude Code)      (Codex CLI)
```

## Architecture

There is exactly **one realtime voice loop** and an **async worker pool** for background agent jobs. Those are the two halves of the binary.

### The voice loop (`src/voice_loop/`)

A single `tokio::select!` loop that owns:

- a microphone stream (`cpal`, mono, resampled to 24 kHz)
- a websocket to the OpenAI Realtime API
- a playback buffer (`cpal` again, jittered)
- a channel of job-completion events from the worker pool

On startup it sends a `session.update` that registers two tools — `delegate_to_orchestrator` and `sub_agent_progress` — and tells the model to use stable snake_case **slugs** for each background task. Reusing a slug continues the same orchestrator conversation; new slugs spawn parallel work. Run `gamechat --print-realtime-config` to inspect the exact JSON.

### The worker pool (`src/orchestrator/`)

`OrchestratorJobManager` runs in its own task. Behind it:

- **One worker per slug.** All sends for `refactor_docs` go through the same `OrchestratorSession`, in order. Different slugs run concurrently in independent sessions.
- **A `ProgressStore`.** Workers stream snippets into a slug-keyed buffer; `sub_agent_progress` queries it with built-in rate limiting (~5 s) so the model can't poll itself into a loop.
- **A `Provider` / `Session` trait** with two backends:
  - **`claude`** — spawns `claude -p` per send. First send uses `--name <slug>`; subsequent sends use `--resume <session_id>`. Claude Code's server-side prompt cache does the heavy lifting; we don't maintain our own.
  - **`codex`** — spawns `codex exec` per send and tails its output back through the same `SendResult` shape.

Adding a third backend means implementing `Provider` and wiring one match arm in `main.rs`. The voice loop doesn't know which agent is on the other end.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/zdql/gamechat/main/install.sh | sh
```

The installer downloads the right prebuilt binary for your platform from the latest GitHub release and drops it in `~/.local/bin/gamechat`. The first release tarball gets built automatically by `.github/workflows/release.yml` when you push a `v*` tag.

### From source

```sh
git clone <repo-url> gamechat && cd gamechat
cargo install --path .
```

Requires Rust 1.87+.

## Prerequisites

- **`OPENAI_API_KEY`** — required. The Realtime API runs on OpenAI regardless of which background agent you use.
- **A coding agent on your `$PATH`:**
  - [Claude Code] (`claude`) — default backend.
  - [Codex CLI] (`codex`) — pass `--provider codex`.
- Microphone + speakers.

A `.env` file in the current or any parent directory is loaded automatically.

## Usage

```sh
# Live voice session — talks to Claude Code by default. Ctrl-C to stop.
gamechat --realtime

# Use Codex instead.
gamechat --realtime --provider codex --codex-model gpt-5-codex

# One-shot delegation (no audio) — useful for scripting / smoke tests.
gamechat --once "summarize the last 5 commits" --slug summarize_commits

# Dump the session.update JSON without connecting.
gamechat --print-realtime-config
```

| Flag | Default | Description |
|------|---------|-------------|
| `--realtime` | — | Live voice loop (mic + speakers). |
| `--once <msg>` | — | Single delegation; prints the `VoiceUpdate` JSON. |
| `--provider <claude\|codex>` | `claude` | Background coding agent. |
| `--model <id>` | `gpt-realtime-2` | Realtime voice model. |
| `--slug <slug>` | `default` | Background task slug (used with `--once`). |
| `--claude-bin` / `--claude-model` | autodetect | Override the `claude` binary or its model. |
| `--codex-bin` / `--codex-model` | autodetect | Override the `codex` binary or its model. |

## Repository layout

```
gamechat/
├── src/
│   ├── main.rs              # CLI, dotenv, provider wiring
│   ├── types.rs             # DelegateToOrchestratorArgs, VoiceUpdate
│   ├── voice_loop/
│   │   ├── mod.rs           # The select! loop
│   │   ├── session.rs       # session.update JSON + tool defs
│   │   └── audio.rs         # cpal input/output, resampling
│   └── orchestrator/
│       ├── interface.rs     # Provider / Session traits, public types
│       ├── jobs.rs          # OrchestratorJobManager + per-slug workers
│       ├── progress.rs      # ProgressStore + rate-limited snapshots
│       ├── bridge.rs        # Realtime tool calls ↔ job events
│       ├── shared.rs        # Logging / stream helpers
│       ├── claude/          # `claude -p` backend
│       └── openai/          # `codex exec` backend
├── install.sh               # curl|sh installer
└── .github/workflows/release.yml  # cross-platform release builds
```

## License

MIT.

[Claude Code]: https://github.com/anthropics/claude-code
[Codex]: https://github.com/openai/codex
[Codex CLI]: https://github.com/openai/codex
