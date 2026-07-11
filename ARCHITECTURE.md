# hcom Architecture

## Purpose

hcom is a CLI tool that connects AI coding agents — Claude Code, Gemini CLI, Codex, OpenCode, Pi, Oh My Pi, Cursor, Kimi, Copilot, Antigravity, and Kilo Code — so they can message, watch, and spawn each other across terminals. It works as a transparent PTY wrapper: agents run inside pseudo-terminals that hcom controls, allowing it to inject messages, observe screen output, and detect gate conditions (ready prompts, approval dialogs, user activity) without modifying the tools themselves. A single Rust binary, no background services, zero configuration required.

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                       hcom CLI (router.rs)                      │
│                                                                 │
│  ┌──────────┐  ┌──────────────┐  ┌──────────┐  ┌────────────┐ │
│  │   TUI    │  │    Commands   │  │  Launch  │  │ PTY Proxy  │ │
│  │(ratatui) │  │ (send, list,  │  │  Agent   │  │(pty/mod.rs │ │
│  │          │  │  config, ...) │  │          │  │  win.rs)   │ │
│  └──────────┘  └──────────────┘  └──────────┘  └─────┬──────┘ │
│                                                       │        │
│  ┌────────────────────────────────────────────────────┐│        │
│  │              Hook Handlers (hooks/)                ││        │
│  │  ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐ ┌────────┐  ││        │
│  │  │Claude│ │Gemini│ │Codex │ │OpenCd│ │ ... 11 │  ││        │
│  │  └──────┘ └──────┘ └──────┘ └──────┘ └────────┘  ││        │
│  └────────────────────────┬───────────────────────────┘│        │
│                           │                             │        │
│  ┌────────────────────────▼─────────────────────────────┘        │
│  │                    SQLite Database (db/)                       │
│  │  instances | events | notify_endpoints | kv | subscriptions   │
│  └────────────────────────┬──────────────────────────────────────┘
│                           │
│  ┌────────────────────────▼──────────────────────────────────────┐
│  │  Delivery Loop (delivery.rs) — PTY text injection via TCP     │
│  │  Screen State → Gate Check → inject_text() → inject_enter()  │
│  └───────────────────────────────────────────────────────────────┘
│                                                                   │
│  ┌───────────────────────────────────────────────────────────────┐
│  │  MQTT Relay (relay/) — cross-device sync via MQTT             │
│  │  push/pull events · end-to-end encrypted · replay guard       │
│  └───────────────────────────────────────────────────────────────┘
└─────────────────────────────────────────────────────────────────┘
```

**Data flow at a glance:**

```
Agent A TUI ←→ hcom PTY Proxy ←→ Agent A process
                    ↕
            TCP Injection Server (localhost:N)
                    ↕
            Delivery Loop reads DB, injects text
                    ↕
            Agent B writes event → DB → delivery → Agent A
```

## Subsystem Descriptions

### Router & CLI (`src/router.rs`, `src/commands/`)

The router is the single entry point after `main()`. It parses `argv` and classifies the action into one of:

- **TUI** — no arguments → launches ratatui dashboard
- **PTY** — `hcom pty <tool> [args]` → wraps a tool in a pseudo-terminal
- **Launch** — `hcom [N] claude|gemini|codex|...` → spawns one or more agent instances
- **Command** — `hcom send|list|config|status|...` → native Rust CLI commands
- **Hook** — `hcom sessionstart|poll|...` → tool-specific lifecycle hooks
- **RelayWorker** — `hcom relay-worker` → background MQTT relay process
- **NewTerminal** — `hcom --new-terminal` → opens TUI in a fresh terminal window

The router uses clap for global flag extraction (`--name`, `--go`) and manual dispatch for command routing, because hcom's CLI is unusual: hook names appear as bare subcommands, launch commands can start with a numeric count (`hcom 3 claude`), and ~40 hook/command/tool names need classification.

The `HCOM_DEV_ROOT` mechanism lets a development worktree replace the installed binary transparently: if the env var or DB key is set, the router re-execs into the worktree's `target/debug/hcom` or `target/release/hcom`.

### PTY Proxy (`src/pty/mod.rs`, `src/pty/win.rs`)

The PTY proxy is the backbone of hcom's agent integration. Every tool launched via `hcom <tool>` runs inside a pseudo-terminal that hcom controls, rather than directly in the user's terminal. This gives hcom the ability to:

- **Inject text** — messages arrive as keystrokes injected into the PTY input
- **Read output** — screen content is parsed by a vt100 emulator for gate detection
- **Forward signals** — SIGINT, SIGTERM, SIGHUP, SIGWINCH pass through transparently
- **Track state** — ready patterns, approval dialogs, input box text, user activity

**Unix** (`src/pty/mod.rs`): Uses `openpty()` from `nix::pty` + `poll()` for I/O multiplexing. A single-threaded poll loop reads from the PTY master, stdin, inject server, and inject clients, then writes coalesced output to stdout. Signal handlers set atomic flags checked each iteration.

**Windows** (`src/pty/win.rs`): Uses `portable-pty` (ConPTY) with a multi-threaded architecture: reader thread (PTY output → stdout), inject thread (TCP → PTY input), stdin thread, resize watcher, and a delivery coordinator thread. The coordinator owns delivery-thread startup (ready-or-timeout gated) since the reader blocks in `read()` and cannot run a timer.

See [`docs/pty.md`](docs/pty.md) for the full PTY subsystem documentation.

### Screen Tracker (`src/pty/screen.rs`)

A vt100-based terminal emulator that processes PTY output byte-by-byte to track:

- **Ready detection** — is the tool's prompt pattern visible on screen?
- **Approval detection** — is a permission dialog blocking input? (per-tool: Claude/Gemini/Codex via OSC titles; Antigravity/Cursor via screen scraping; Codex additionally via OSC9 title parsing)
- **Input box extraction** — what text is in the prompt? (tool-specific: Claude uses `❯` with vt100 dim-attribute placeholder detection; Gemini uses bordered boxes; Codex uses `›`; etc.)
- **Stability detection** — has screen output settled? (debounce gate conditions)
- **Debug dumps** — periodic screen snapshots via `pty_debug` flag file

The tracker maintains a rolling 4KB output buffer for OSC title parsing (approval detection) and forwards output to the vt100 parser for full terminal state. The parser is wrapped in `catch_unwind` to survive upstream vt100-rust panics (known issue: wide-character resize corruption).

### Delivery Loop (`src/delivery.rs`)

The delivery loop is the nervous system that pushes messages from the SQLite database into agents' PTY sessions. It runs as a dedicated thread per agent and implements a state machine:

```
Idle → Pending → WaitTextRender → WaitTextClear → VerifyCursor → Idle
```

1. **Idle** — waits on a TCP notify port for new events (30s timeout, also used for sleep/wake detection)
2. **Pending** — evaluates delivery gate conditions (see below); if safe, builds `<hcom>...</hcom>` wake text and injects it
3. **WaitTextRender** — polls screen tracker to confirm injected text appears in the prompt
4. **WaitTextClear** — sends Enter, verifies prompt cleared (with retries)
5. **VerifyCursor** — checks hook-side cursor advanced (message delivered), falls back to pending check

**Delivery gate** — a sequence of checks that answers "is it safe to inject right now?":
- `require_idle` — DB status must be "listening" (agent not processing a turn)
- `block_on_approval` — no approval dialog blocking the prompt
- `block_on_user_activity` — no keystrokes within cooldown (0.5s default)
- `submit_settle` — cooldown after prompt submit (1.5s, covers hook→status race)
- `require_ready_prompt` — ready pattern visible on screen
- `require_prompt_empty` — no uncommitted user text in the input box

For OpenCode, Pi, OMP, and Kilo, the delivery loop uses a simplified "first-message bootstrap" mode: it injects one PTY message to create the TUI session, then plugin-based delivery takes over.

### Hook Handlers (`src/hooks/`)

Hooks are per-tool lifecycle event handlers that the AI tool invokes at specific points (session start, poll, message send, status change). Each tool has its own hook interface:

- **Claude** (`hooks/claude.rs`) — `sessionstart`, `poll`, `UserPromptSubmit`, etc.
- **Gemini** (`hooks/gemini.rs`) — `gemini-beforeagent`, `gemini-afteragent`, `sessionstart`
- **Codex** (`hooks/codex.rs`) — `codex-sessionstart`, `codex-poll`
- **OpenCode** (`hooks/opencode.rs`) — `opencode-start`, `opencode-message`
- **Pi** (`hooks/pi.rs`) — `pi-start`, `pi-message`
- **OMP** (`hooks/omp.rs`) — `omp-start`, `omp-message`
- **Cursor** (`hooks/cursor.rs`) — `cursor-sessionstart`
- **Kimi** (`hooks/kimi.rs`) — `kimi-sessionstart`
- **Copilot** (`hooks/copilot.rs`) — `copilot-sessionstart`, `copilot-poll`
- **Antigravity** (`hooks/antigravity.rs`) — shares Gemini hook dispatch

The `HookHandler` trait (`hooks/handler.rs`) provides a unified dispatch framework: build context → open DB → gate check → strip argv → dispatch → log timing → return. This eliminates ~30 lines of boilerplate per tool.

**Hook flow:**

```
Tool lifecycle event
       │
       ▼
Tool invokes hook binary (e.g., hcom sessionstart)
       │
       ▼
router.rs → Action::Hook → dispatch_hook_for_tool()
       │
       ├── Claude   → hooks/claude.rs
       ├── Gemini   → hooks/gemini.rs
       ├── Codex    → hooks/codex.rs
       ├── OpenCode → hooks/opencode.rs
       ├── Pi       → hooks/pi.rs
       ├── OMP      → hooks/omp.rs
       ├── Cursor   → hooks/cursor.rs
       ├── Kimi     → hooks/kimi.rs
       ├── Copilot  → hooks/copilot.rs
       └── Antigravity → hooks/gemini.rs (shared)
              │
              ▼
         HookHandler trait (handler.rs):
         build_ctx() → open_db() → gate_check() → dispatch() → log()
              │
              ▼
         Write event to DB (message, status, lifecycle)
              │
              ▼
         Notify delivery threads via TCP notify port
```

### SQLite Database (`src/db/`)

A single SQLite database (`~/.hcom/hcom.db` on Unix, `%HCOM_DIR%/hcom.db` on Windows) with WAL mode and foreign keys. Three loosely-coupled state planes:

- **Instances** (`instances` table) — live per-agent state: name, status, session_id, tag, PID, cursor position, directory, tool type, launch context, terminal presets, process/session bindings
- **Events** (`events` table) — append-only history: messages, status changes, lifecycle events. Includes an `events_v` view with extracted JSON fields for querying, an FTS5 full-text search index, and an insert trigger that populates the search index automatically.
- **Control-plane state** — `notify_endpoints` (TCP ports for waking delivery threads), `process_bindings` (process ID → instance name), `session_bindings` (session ID → instance name), `kv` (generic key-value store for config, relay state, subscriptions), `subscriptions` (event subscription rules)

Schema migration uses version-checked `PRAGMA user_version`. On mismatch, the DB is archived (copied to `archive/session-{timestamp}/`) and recreated fresh. Running instances are snapshotted to pidtrack before archive so orphan recovery re-registers them.

### MQTT Relay (`src/relay/`)

Cross-device coordination via MQTT pub/sub. The relay syncs instance state and events across devices:

- **Topic layout**: `{relay_id}/{device_uuid}` (retained state per device), `{relay_id}/control` (non-retained control events like stop/kill)
- **End-to-end encryption**: XChaCha20-Poly1305 with shared PSK, binding each payload to relay, topic, and timestamp. Replay guard drops duplicate envelopes within a freshness window.
- **Push/pull**: The relay worker maintains an MQTT connection, pushes local events (periodically and on notification), and pulls remote events to insert into the local DB. Control messages (stop, kill, spawn) are handled immediately.
- **Health derivation** (`relay/mod.rs`): Pure-function health state machine: `RelayObservation` (raw signals) → `derive_relay_health()` → `RelayHealth` enum (NotConfigured, Disabled, Waiting, Starting, Connected, Stale, Error with sub-reasons).
- **Daemon mode**: A relay daemon can handle polling on a persistent port; `trigger_push()` wakes it via TCP connect.

### TUI Dashboard (`src/tui/`)

A ratatui-based terminal dashboard opened by `hcom` with no arguments. Provides:

- Agent list with live status, tool, and message preview
- Message history per agent
- Agent launch dashboard
- Inline command input for `hcom send`, `hcom kill`, etc.
- Theme support
- Status event log

The TUI reads from the SQLite DB and does not require any agent to be running.

### GUI (`gui/`)

A Tauri v2 desktop GUI application. Separate from the Rust binary; provides a graphical alternative to the TUI dashboard. The GUI reads from the same SQLite database and connects via the same TCP notify/inject ports.

## Data Flow: Message from Agent A to Agent B

```
1. Agent A's tool invokes hcom message hook (or user runs `hcom send @B -- hello`)
2. Hook handler writes message event to SQLite events table
3. Hook handler updates instance B's cursor (last_event_id)
4. NotifyServer sends TCP notification to instance B's delivery loop
5. Delivery loop wakes from notify.wait() and checks for pending messages
6. Delivery gate evaluates: is agent B idle? prompt empty? no approval blocking?
7. If safe, builds `<hcom>[intent] sender -> recipient</hcom>` wake text
8. inject_text(port, wake_text) — TCP connect to agent B's inject server
9. inject_enter(port) — submits the text via TCP
10. WaitTextRender phase: polls screen tracker until injected text appears
11. WaitTextClear phase: verifies prompt cleared (injection took effect)
12. VerifyCursor phase: checks hook-side cursor advanced → message delivered
13. Agent B's hooks see the injected <hcom> tag and process the message
```

For tools with plugin delivery (OpenCode, Pi, OMP), step 5 is replaced: the plugin-host's TypeScript extension picks up messages via the hcom plugin API and renders them directly in the TUI.

## Platform Support

| Feature | Unix (macOS, Linux) | Windows | Termux/Android |
|---------|---------------------|---------|----------------|
| PTY backend | `openpty()` via nix | ConPTY via portable-pty | `openpty()` (Linux kernel) |
| I/O model | Single-threaded poll loop | Multi-threaded (reader, inject, stdin, resize, coordinator) | Same as Unix |
| Signal handling | SIGWINCH, SIGINT, SIGTERM, SIGHUP via `sigaction` | No SIGWINCH; resize watcher polls every 200ms | Same as Unix |
| Terminal size | `ioctl(TIOCGWINSZ)` | `crossterm::terminal::size()` | `ioctl(TIOCGWINSZ)` |
| Tool launcher | Direct exec | PowerShell runner script (shim via `.cmd`/`.bat` detection) | Termux-specific launcher override |
| Schema guard | Unix unlink works on open files | Windows needs handle release before DB archive | Same as Unix |

**Key Windows differences:**
- PowerShell → native-exe boundary corrupts arguments with embedded double quotes → JSON sidecar file for args
- `cmd.exe` / `.bat` shims need `cmd.exe /c` wrapper (npm-installed tools)
- ConPTY output filter (`OutputModeFilter` in `pty/shared.rs`) strips Win32-input mode-sets (`?9001`) and focus reporting (`?1004`) from child output so the outer terminal stays in VT mode
- Headless mode requires synthetic DSR reply (`ESC[1;1R`) since there's no real terminal to answer

**Termux/Android:**
- Uses the Unix code path with `termux-wake-lock` and `am start` for terminal launching
- Tools may need a launcher override instead of direct exec

## Key Design Decisions

### Why PTY proxy instead of API-based integration?
Each AI tool has a different extension API (OpenCode plugins, Codex events, Claude hooks, Gemini extensions). The PTY approach works uniformly across all of them: run the tool in a controlled terminal, inject keystrokes, scrape the screen. No tool-specific API needed for the core messaging loop — hooks are only needed for lifecycle events (session start/end, message delivery confirmation).

### Why SQLite?
SQLite is embedded (no server process), supports concurrent readers (WAL mode), and stores both live state (instances) and history (events) in one file. It's the same database accessed by all agent processes, the TUI, and the relay — no network, no daemon. Schema migration and auto-archive handle version changes gracefully.

### Why no background service / daemon?
hcom is intentionally a process-per-agent tool. Each `hcom <tool>` invocation is its own process with its own PTY, delivery thread, and DB connection. This keeps the model simple: kill the process, kill the agent. The only exception is the relay worker (`hcom relay-worker`), which runs as a background daemon for MQTT.

### Why TCP for text injection (not stdin)?
The delivery loop runs in a separate thread from the PTY's main loop. TCP to localhost is the cleanest cross-thread boundary: the delivery thread connects to `127.0.0.1:N`, writes text, and disconnects. The inject server (attached to the PTY master's poll loop) picks up the data and writes it to the PTY. This also allows external tools (`hcom send --inject`, `hcom term inject`) to inject text without going through the delivery state machine.

### Why vt100 screen scraping instead of structured output?
AI tools render rich TUI output (Ink/React, bubbletea, custom TUIs). There is no machine-readable output stream. The vt100 emulator reconstructs the terminal screen state, which is the only universal interface available. Per-tool input extraction is needed because each tool renders its prompt differently (❯, ›, >, →, bordered boxes...).

### Why separate delivery state machine?
The gate checks prevent injection at unsafe moments (user typing, approval dialog, mid-turn processing). The verification phases (WaitTextRender → WaitTextClear → VerifyCursor) ensure the message actually landed before declaring success. Without this, messages would be lost in unread prompt buffers or clobber approval dialogs.

## Project Structure

```
hcom/
├── Cargo.toml                  # Rust project manifest
├── README.md                   # User-facing documentation
├── ARCHITECTURE.md             # This file
├── docs/
│   └── pty.md                  # PTY subsystem documentation
├── gui/                        # Tauri v2 desktop GUI (TypeScript)
├── scripts/                    # Bundled workflow scripts (confess, debate, fatcow)
├── plugin/                     # Tool plugin extensions
├── install.sh                  # Shell installer
├── pyproject.toml              # PyPI distribution config
├── Justfile                    # Task runner (ci, ci-windows)
│
└── src/
    ├── main.rs                 # Entry point: panic hook, dispatch to router
    ├── router.rs               # CLI routing: TUI, PTY, Launch, Command, Hook
    ├── bootstrap.rs            # Agent CLI primer (~700 tokens of hcom docs)
    ├── cli_context.rs          # Command context builder (identity, gate checks)
    ├── config.rs               # Global config (env vars, config.toml)
    │
    ├── pty/                    # PTY proxy subsystem
    │   ├── mod.rs              # Unix openpty + poll loop (primary implementation)
    │   ├── win.rs              # Windows ConPTY via portable-pty
    │   ├── shared.rs           # Portable orchestration (delivery start, approval, screen refresh)
    │   ├── screen.rs           # vt100 screen tracker (ready, approval, input text)
    │   ├── inject.rs           # TCP injection server (localhost random port)
    │   └── terminal.rs         # Unix raw mode + signal handlers
    │
    ├── delivery.rs             # Message delivery loop (state machine, gate, inject)
    ├── delivery/
    │   └── antigravity.rs      # Antigravity-specific delivery handling
    │
    ├── hooks/                  # Per-tool lifecycle hook handlers
    │   ├── handler.rs          # HookHandler trait + unified dispatch
    │   ├── common.rs           # Shared hook utilities
    │   ├── claude.rs           # Claude Code hooks
    │   ├── gemini.rs           # Gemini CLI / Antigravity hooks
    │   ├── codex.rs            # Codex CLI hooks
    │   ├── codex_file_edits.rs # Codex transcript file watcher
    │   ├── opencode.rs         # OpenCode / Kilo hooks
    │   ├── pi.rs               # Pi hooks
    │   ├── omp.rs              # Oh My Pi hooks
    │   ├── cursor.rs           # Cursor Agent hooks
    │   ├── kimi.rs             # Kimi hooks
    │   ├── copilot.rs          # Copilot CLI hooks
    │   ├── antigravity.rs      # Antigravity hooks
    │   ├── family.rs           # Tool family detection
    │   └── utils.rs            # Hook utilities
    │
    ├── db/                     # SQLite database layer
    │   ├── mod.rs              # HcomDb: open, schema, migration, archive
    │   ├── events.rs           # Event/message CRUD
    │   ├── instances.rs        # Instance state management
    │   ├── kv.rs               # Generic key-value store
    │   ├── notify.rs           # Notify endpoint registration
    │   ├── sessions.rs         # Session bindings
    │   ├── subscriptions.rs    # Event subscriptions (threads, membership)
    │   └── reqwatch_policy.rs  # Request-watch policy
    │
    ├── relay/                  # MQTT cross-device relay
    │   ├── mod.rs              # Topics, PSK, health derivation, relay status
    │   ├── client.rs           # MQTT client wrapper
    │   ├── broker.rs           # Broker discovery
    │   ├── push.rs             # Local event push
    │   ├── pull.rs             # Remote event pull
    │   ├── control.rs          # Control message handling (stop/kill/spawn)
    │   ├── crypto.rs           # XChaCha20-Poly1305 encryption
    │   ├── token.rs            # Join token generation
    │   ├── replay.rs           # Replay guard
    │   └── worker.rs           # Background relay daemon
    │
    ├── tui/                    # Ratatui terminal dashboard
    │   ├── mod.rs, app.rs      # App structure
    │   ├── data.rs, db.rs      # Data fetching
    │   ├── model.rs, state.rs  # Application state
    │   ├── input.rs             # Input handling
    │   ├── input/compose.rs    # Multi-line input composition
    │   ├── render/             # Rendering modules (agents, messages, launch, text)
    │   ├── inline/             # Inline display modes
    │   ├── theme.rs            # Theme system
    │   ├── status.rs           # Status bar
    │   ├── actions.rs          # Action dispatch
    │   ├── commands.rs         # TUI command execution
    │   ├── rpc.rs, rpc_async.rs # TUI RPC to agent PTYs
    │   └── inline/eject.rs    # Inline-to-fullscreen transition
    │
    ├── commands/               # Native CLI command implementations
    │   ├── mod.rs              # Command module
    │   ├── launch.rs           # Agent launch
    │   ├── send.rs             # Message send
    │   ├── list.rs             # Agent listing
    │   ├── status.rs           # Diagnostics
    │   ├── config.rs           # Configuration
    │   ├── hooks.rs            # Hook management
    │   ├── kill.rs             # Agent termination
    │   ├── resume.rs / fork.rs # Session management
    │   ├── start.rs / stop.rs  # Session lifecycle
    │   ├── listen.rs           # Ad-hoc agent listen
    │   ├── events.rs           # Event subscription
    │   ├── transcript.rs       # Transcript viewing
    │   ├── term.rs             # PTY terminal view + inject
    │   ├── relay.rs            # Relay management
    │   ├── run.rs              # Workflow script execution
    │   ├── bundle.rs           # Message bundling
    │   ├── update.rs           # Self-update
    │   ├── reset.rs            # DB reset
    │   ├── reset_ops.rs        # Reset operations
    │   ├── reset_preview.rs    # Reset preview
    │   ├── archive.rs          # DB archive query
    │   ├── daemon.rs           # Relay daemon management
    │   └── help.rs             # Help system
    │
    ├── integration_spec.rs     # Tool integration specifications
    ├── tool.rs                 # Tool enum + metadata
    ├── launcher/               # Agent launcher subsystem
    │   ├── mod.rs              # Launch pipeline orchestration
    │   ├── preprocess.rs       # Arg preprocessing
    │   ├── environment.rs      # Environment setup
    │   ├── pipeline.rs         # Batch launch pipeline
    │   └── batch.rs           # Multi-agent batch launch
    ├── tools/                  # Tool-specific preprocessing
    │   ├── mod.rs              # Tool preprocessing
    │   ├── codex_preprocessing.rs
    │   ├── copilot_preprocessing.rs
    │   ├── cursor_preprocessing.rs
    │   ├── opencode_preprocessing.rs
    │   ├── launch_arg_validation.rs
    │   └── args_common.rs     # Shared arg helpers
    ├── terminal/               # Terminal emulator / window management
    │   ├── mod.rs              # Terminal abstraction
    │   ├── presets.rs          # Terminal presets (kitty, wezterm, tmux, etc.)
    │   ├── spawn.rs            # Process spawning
    │   ├── kill.rs             # Terminal pane closing
    │   ├── scripts.rs          # Terminal runner scripts
    │   └── windows.rs          # Windows-specific terminal handling
    │
    ├── transcript/             # Per-tool transcript parsing
    │   ├── mod.rs              # Transcript trait
    │   ├── claude.rs, codex.rs, copilot.rs, cursor.rs
    │   ├── gemini.rs, kimi.rs, opencode.rs, pi.rs
    │   └── shared.rs           # Shared transcript utilities
    │
    ├── notify/                 # TCP notification system
    │   ├── mod.rs              # Notify trait
    │   ├── server.rs           # Notify server (TCP listener)
    │   └── wake.rs             # Notify wake client
    │
    ├── identity.rs             # Agent identity resolution
    ├── instance_names.rs       # Name generation (CVCV hash names)
    ├── instances.rs            # Instance management
    ├── instance_binding.rs     # Process-to-instance binding
    ├── instance_lifecycle.rs   # Instance lifecycle (launch → ready → stop)
    ├── messages.rs             # Message types
    ├── scripts.rs              # Workflow script runner
    ├── runtime_env.rs          # Runtime environment detection
    ├── shell_env.rs            # Shell environment management
    ├── paths.rs                # Path resolution
    ├── pidtrack.rs             # PID tracking
    ├── log.rs                  # Structured logging
    ├── update.rs               # Self-update mechanism
    ├── sys/                    # Platform-specific system calls
    │   ├── mod.rs, fs.rs, io.rs, net.rs, process.rs, signal.rs
    └── shared/                 # Shared types and utilities
        ├── mod.rs, ansi.rs, constants.rs, context.rs, errors.rs
        ├── identity.rs, platform.rs, terminal_presets.rs, time.rs
        └── tool_detection.rs
```
