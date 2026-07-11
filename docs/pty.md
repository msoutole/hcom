# PTY Proxy Subsystem

## Overview

The PTY proxy is the core mechanism that gives hcom control over AI coding tool processes. Instead of running tools directly in the user's terminal, hcom spawns them inside a pseudo-terminal (PTY). This indirection enables three critical capabilities:

1. **Text injection** — messages arrive as keystrokes written to the PTY master, appearing exactly as if the user typed them
2. **Screen observation** — the PTY master output is fed through a vt100 terminal emulator that tracks ready state, approval dialogs, and input box content
3. **Signal forwarding** — terminal signals (SIGINT, SIGTERM, SIGWINCH) pass through transparently

The PTY subsystem is the most complex part of hcom (~25% of the codebase) because it must handle two fundamentally different platform backends (Unix `openpty` and Windows ConPTY), per-tool screen scraping, escape sequence filtering, and a TCP-based text injection protocol — all while maintaining a seamless user experience where the tool appears to run in a normal terminal.

## Architecture

```
User Terminal ←→ hcom PTY Proxy ←→ AI Tool Process (inside PTY)
                     ↕
               TCP Inject Server (127.0.0.1:random)
                     ↕
               Delivery Loop (reads SQLite, injects text)
                     ↕
               SQLite Database (message bus)
```

The user sees and interacts with the tool through the PTY proxy: stdin from the user's terminal is forwarded to the PTY master, and PTY master output is forwarded to the user's stdout. Independently, a delivery thread reads pending messages from SQLite and injects them into the PTY via TCP when gate conditions are safe.

## Unix Implementation (`src/pty/mod.rs`)

### openpty() Setup

The Unix proxy uses `nix::pty::openpty()` to create a pseudo-terminal pair. The slave side becomes the tool's controlling terminal (via `setsid()` + `TIOCSCTTY` + `dup2` for stdin/stdout/stderr), while the master side is held by hcom for I/O forwarding.

```
openpty(&winsize)
    ↓
pty.master (hcom reads/writes)
pty.slave  → dup2(slave, 0/1/2) → tool process
    ↓
setsid() → TIOCSCTTY → dup2(slave_fd, 0..2) → close(master) → exec(tool)
```

The `pre_exec` closure in `Command::spawn` runs after `fork()` but before `exec()` — it's restricted to async-signal-safe operations only.

### Poll-Based I/O Multiplexing

The main loop uses a single `poll()` call with these file descriptors:

| FD | Direction | Purpose |
|----|-----------|---------|
| PTY master | Read | Tool's stdout/stderr output |
| stdin | Read | User keystrokes |
| Inject listener | Read | New TCP injection connections |
| Inject clients (N) | Read | Data from connected injectors |

On each iteration, the loop:

1. Checks signal flags (SIGWINCH, SIGINT, SIGTERM, SIGHUP)
2. Calls `poll()` with 5-10s timeout (debug enabled) or blocks indefinitely
3. Drains PTY master output in a coalescing loop (reads until EAGAIN, with one 1ms retry for trailing fragments — prevents TUI flicker from split frames)
4. Applies `TitleOscFilter` to strip the tool's own title OSCs (0, 1, 2) from stdout
5. Forwards coalesced, filtered output to stdout in a single `write()`
6. Feeds raw (unfiltered) output to the `ScreenTracker` for vt100 parsing
7. Reads stdin, strips focus events (for Copilot), forwards to PTY master
8. Accepts new inject connections and reads inject client data

### Signal Handling

POSIX signal handlers set atomic flags checked on each poll iteration:

- **SIGWINCH** — `forward_winsize()`: reads terminal size via `ioctl(TIOCGWINSZ)`, calls `ioctl(TIOCSWINSZ)` on the PTY master, and sends SIGWINCH to the child process group
- **SIGINT** — forwarded to the child process group as SIGINT
- **SIGTERM** — sets `EXIT_WAS_KILLED` and breaks the main loop, triggering Drop cleanup
- **SIGHUP** — terminal closed; breaks the loop without forwarding (the child gets its own SIGHUP when the PTY tears down)

### Title OSC Filtering

The `TitleOscFilter` state machine strips OSC 0/1/2 (title/icon sequences) from the tool's output before forwarding to the user's terminal. hcom replaces them with its own title showing agent name and status. The filter:

- Maintains state across read boundaries (ESC, `]`, digit, `;`, title content, BEL/ST terminator)
- Only discards title bytes — real output passes through immediately (no buffering delay)
- Has a 256-byte safety abort on unterminated sequences
- Flushes held prefix bytes on EOF

### Key Patterns

- **Escape sequence boundary safety**: `has_pending_escape()` scans backwards for the last ESC byte and classifies the incomplete sequence type (CSI, OSC, DCS/PM/APC, single-shift, nF). `resolve_pending_escape()` checks continuation chunks for type-appropriate terminators. Title writes are deferred while a sequence is incomplete.
- **UTF-8 boundary safety**: `pending_utf8_bytes()` checks the last 1-3 bytes of coalesced output for incomplete multi-byte sequences. Title writes are deferred until the sequence completes.
- **Focus event stripping**: `strip_focus_events()` removes `CSI I` / `CSI O` (focus in/out) from forwarded stdin for Copilot, which pauses stdin draining on focus-out.
- **macOS kqueue workaround**: A `listener_backoff` flag skips the inject listener in one `poll()` iteration when `accept()` returns `WouldBlock`, preventing kqueue's spurious POLLIN busy-loop.

## Windows Implementation (`src/pty/win.rs`)

Windows has no `openpty()` or `poll()` for PTY masters. Instead, hcom uses `portable-pty` (a cross-platform crate that wraps ConPTY via `CreatePseudoConsole`). The I/O model shifts from single-threaded poll to five dedicated threads:

### Thread Architecture

```
                        ┌──────────────────┐
                        │  Delivery        │
                        │  Coordinator     │──→ starts delivery when ready/timeout
                        └──────────────────┘
                                 │
┌──────────┐   ┌──────────┐   ┌──────────────┐   ┌──────────┐   ┌────────────────┐
│  Reader  │   │  Stdin   │   │    Inject    │   │  Resize  │   │   Delivery     │
│  Thread  │   │  Thread  │   │   Thread     │   │  Watcher │   │   Thread       │
│          │   │          │   │              │   │          │   │                │
│ PTY→stdout│   │ stdin→PTY│   │ TCP→PTY     │   │ poll size│   │ DB→TCP inject  │
│ scratch  │   │          │   │ screen qry   │   │→resize   │   │ gate checks    │
│ snapshot │   │          │   │              │   │          │   │                │
└──────────┘   └──────────┘   └──────────────┘   └──────────┘   └────────────────┘
```

1. **Reader thread** — blocks in `reader.read()` on the ConPTY output pipe, forwards filtered output to stdout, feeds the ScreenTracker, maintains an eagerly-refreshed screen snapshot for `hcom term` queries, and applies pending resizes. Uses a producer-consumer split (sync_channel) so the consumer can `recv_timeout()` instead of blocking forever — this lets it render a final `hcom term` frame ~120ms after an idle agent's output stops.

2. **Stdin thread** — blocks in `stdin.read()`, forwards keystrokes to the ConPTY writer. Detached (never joined) because `stdin.read()` may outlive the child process; `std::process::exit()` reaps it.

3. **Inject thread** — polls the InjectServer for new TCP connections and reads client data. Serves screen queries from the snapshot (so idle agents answer immediately). Enforces FIFO order with a 2s stall timeout to prevent a stuck client from starving later ones. On injected text that clears an approval, requests a tracker-clear from the reader thread via an atomic.

4. **Resize watcher** — polls `crossterm::terminal::size()` every 200ms (Windows has no SIGWINCH), forwards changes to the ConPTY via `MasterPty::resize()` and to the ScreenTracker via `pending_resize` atomic.

5. **Delivery coordinator** — polls every 50ms and starts the delivery thread when the tool is ready, the start-timeout elapses, or shutdown begins. The reader thread cannot own this because it blocks in `read()`.

### Key Differences from Unix

| Aspect | Unix | Windows |
|--------|------|---------|
| PTY primitive | `openpty()` from nix | `portable-pty` crate wrapping ConPTY |
| I/O multiplexing | Single-threaded `poll()` | 5 dedicated threads |
| Output filtering | `TitleOscFilter` (inline) | `OutputModeFilter` (stateful, strips Win32-input modes `?9001` and focus `?1004`) |
| DSR reply (headless) | Outer terminal answers | Reader must synthesize `ESC[1;1R` |
| Process reaping | SIGHUP on PTY close | Job object with `KILL_ON_JOB_CLOSE` |
| Tool launcher | Direct `exec()` | PowerShell runner script; `.cmd`/`.bat` → `cmd.exe /c` |
| Resize detection | `SIGWINCH` signal handler | 200ms poll via `crossterm` |
| Title OSC safety | `pending_utf8` + `pending_escape` | `OutputModeFilter::title_write_safe()` |

### OutputModeFilter (`src/pty/shared.rs`)

The `OutputModeFilter` is a stateful parser that rewrites the child's output so the outer terminal never switches into Win32 input mode (`ESC[?9001h`) or focus reporting (`ESC[?1004h`). It also:

- Strips title OSC 0/1/2 (like the Unix `TitleOscFilter`)
- Passes through all other escape sequences unchanged
- Latches DSR cursor-position queries (`ESC[6n`) for headless reply
- Tracks split UTF-8 and split escape sequences across reads
- Lives in `pty/shared.rs` (not `win.rs`) so its correctness-critical parsing runs under the host test gate

## Screen Tracker (`src/pty/screen.rs`)

The `ScreenTracker` wraps a `vt100::Parser` and enriches it with hcom-specific detection logic.

### vt100-Based Terminal Emulation

Every byte of PTY output is fed to `vt100::Parser::process()`. The parser maintains a full terminal screen model (rows, cells with attributes, cursor position). The ScreenTracker wraps this in a `catch_unwind` to survive upstream panics (known issue: wide-character resize corruption in vt100-rust).

### Ready Pattern Detection

`is_ready()` scans all screen rows for a tool-specific pattern (e.g., `"? for shortcuts"` for Claude). When the pattern is visible, the tool's prompt is displayed and waiting for input. The pattern disappears when the user types or an overlay is shown. Returns `true` immediately if no ready pattern is configured (for tools without a known prompt marker).

### Approval Detection (Per-Tool)

Each tool has a unique approval detection strategy:

| Tool | Detection Method | Implementation |
|------|-----------------|----------------|
| Claude/Gemini | Hook-based (status = "blocked") | Lifecycle hooks set DB status |
| Codex | OSC terminal title + screen scrape | `is_waiting_approval()` checks `last_osc_title()` for "Action Required"; `is_codex_approval_visible()` catches dialog variants |
| Antigravity | Screen scrape | `is_antigravity_approval_visible()` looks for "Requesting permission for:" + question/control footer |
| Cursor | Screen scrape | `is_cursor_approval_visible()` looks for "Run this command?" + menu options |
| Copilot | PTY detection | Title-based approval via `SawApproval` in the delivery loop |

### Input Box Text Extraction

Each tool renders its prompt differently, requiring per-tool extraction:

- **Claude** — finds `❯` (or `>`) within `─` bordered box; uses vt100 `dim` attribute to distinguish placeholder (dim) from real input (non-dim)
- **Gemini** — bordered input box with three supported formats (`╭` old, half-block `▀`/`▄`, `─` dash)
- **Codex** — `›` prompt with dim-attribute placeholder detection
- **Kimi** — rounded box (`╭ ╰`) with `│ >` prompt
- **Cursor** — `→` prompt with dim placeholder
- **Copilot** — `❯` prompt (no dim attribute, empty input = bare prompt)
- **Antigravity** — `>` prompt with dim-attribute placeholder
- **OpenCode/Kilo/Pi/OMP** — plugin-based delivery, no PTY input detection needed

### Stability Detection

`is_output_stable(ms)` returns true when no PTY output has arrived for N milliseconds. Used to debounce gate conditions: a transient redraw can briefly erase the input box, and stability prevents a false "prompt empty" detection.

### Debug Mode

When `~/.hcom/.tmp/pty_debug_on` exists, the tracker dumps full screen state (rows, cell attributes, cursor, gate conditions) every 5 seconds to `~/.hcom/.tmp/logs/pty_debug/{name}_{pid}.log`. Toggleable at runtime.

## TCP Injection Server (`src/pty/inject.rs`)

The `InjectServer` binds to `127.0.0.1:0` (random port) on startup. The port is registered in the DB (`notify_endpoints` with `kind='inject'`) so the delivery loop and `hcom term` can find it.

### Protocol

| Operation | Method | Behavior |
|-----------|--------|----------|
| Text injection | Connect → send bytes → close | Accumulates data; on EOF, processes and writes to PTY master (strips trailing LF from echo/nc, preserves CR for submit) |
| Screen query | Connect → send `\x00SCREEN` → close | Serves JSON screen dump from the tracked state |
| Unknown query | Connect → send `\x00<cmd>` → close | Returns `"error: unknown command\n"` |

The `\x00` prefix distinguishes query commands from text injection.

### Integration

- **Unix**: The inject server's listener FD and client FDs are added to the `poll()` set in the main loop. `accept()` and `read_client()` run inline during each iteration.
- **Windows**: The inject server runs in its own thread (`spawn_inject_thread`) with a 10ms poll loop. It preserves connection order (FIFO) with a 2s stall timeout.

## Shared PTY Orchestration (`src/pty/shared.rs`)

This module contains portable functions shared by both Unix and Windows proxies. These were extracted from the Unix `Proxy` struct methods so Windows could reuse the exact same logic.

### Delivery State Management

`update_delivery_state()` reads from the ScreenTracker and writes to a shared `ScreenState` struct accessible by both the main loop and the delivery thread:

```rust
pub struct ScreenState {
    pub ready: bool,
    pub approval: bool,
    pub prompt_empty: bool,
    pub input_text: Option<String>,
    pub visible_tail: Option<String>,
    pub last_user_input: Instant,
    pub last_output: Instant,
    pub cols: u16,
    pub last_prompt_submit: Option<Instant>,
    pub approval_scrape_latched: bool,
}
```

### Delivery Thread Lifecycle

`start_delivery_thread()` spawns the delivery loop and returns one of:

- **Started(JoinHandle)** — init succeeded (DB opened, notify server created). Join at shutdown.
- **Disabled** — no instance name; no thread spawned.
- **Pending(JoinHandle, Receiver)** — thread started but init timed out; non-retryable (must join at shutdown).

On the Unix side, start is triggered inline in the poll loop when the tool is ready or the start timeout elapses. On Windows, a dedicated coordinator thread polls every 50ms using `should_start_delivery()` (ready || timeout || shutdown).

### User Activity Tracking

`note_user_keystroke()` records a genuine keystroke against shared state. When a user keystroke answers a standing approval, it clears the block and publishes the `approval_cleared` edge. Cursor is excluded (its approval is authoritative-by-prompt and clears only when the prompt leaves the screen).

`clear_injected_approval_state()` handles approval clearing for injected text (which bypasses stdin and goes through the inject thread). Only acts when approval is currently showing.
