# Cross-Platform KVM + File Transfer App — Complete Build Guide

**Project codename:** `Seam` (rename freely — used throughout as the crate/app name)
**Target:** macOS + Windows, 2 machines, LAN-only, peer-to-peer
**Stack:** Rust (core + daemon) · Tauri v2 (shell) · React + TypeScript (UI)
**Written:** September 2026 — versions current as of this date

---

## Table of Contents

- [Tier 0 — Scope & Definitions](#tier-0--scope--definitions)
- [Tier 1 — Toolchain Setup](#tier-1--toolchain-setup)
- [Tier 2 — IDE & Editor Setup](#tier-2--ide--editor-setup)
- [Tier 3 — Project Scaffolding](#tier-3--project-scaffolding)
- [Tier 4 — Rust Concepts You'll Actually Use](#tier-4--rust-concepts-youll-actually-use)
- [Tier 5 — Architecture](#tier-5--architecture)
- [Tier 6 — Wire Protocol Specification](#tier-6--wire-protocol-specification)
- [Tier 7 — Feature Specifications](#tier-7--feature-specifications)
- [Tier 8 — Frontend Specification](#tier-8--frontend-specification)
- [Tier 9 — Logging & Commenting Standards](#tier-9--logging--commenting-standards)
- [Tier 10 — Build, Run, Release](#tier-10--build-run-release)
- [Tier 11 — OS Permissions & Signing](#tier-11--os-permissions--signing)
- [Tier 12 — Testing Strategy](#tier-12--testing-strategy)
- [Tier 13 — Milestone Roadmap](#tier-13--milestone-roadmap)
- [Tier 14 — Troubleshooting Reference](#tier-14--troubleshooting-reference)
- [Tier 15 — Deferred / Future Work](#tier-15--deferred--future-work)

---

## Tier 0 — Scope & Definitions

### In scope for v1

| Feature | Notes |
|---|---|
| Mouse sharing | Edge-triggered handoff between two screens |
| Keyboard sharing | Full key + modifier state, with remapping |
| Modifier remapping | Per-machine table (Ctrl ↔ Cmd for your Windows keyboard on Mac) |
| Clipboard sync | Text + images, size-capped |
| File transfer | Drag-drop, chunked, resumable, integrity-checked |
| Visual layout editor | Drag machine tiles, snap edges |
| Auto-discovery | mDNS, with manual IP fallback |
| Pairing | 6-digit code, then pinned certs |
| Log viewer | In-app, filterable, copyable |
| Escape hotkey | Force-return control to local machine |
| Lock-to-screen | Toggle to disable edge handoff |
| Reconnect | Automatic, exponential backoff |
| Sleep/wake recovery | Re-establish event tap after Mac sleep |

### Explicitly deferred

- Third machine (architecture must not *prevent* it — see [Tier 15](#tier-15--deferred--future-work))
- Cross-network / internet relay
- Linux support
- Drag-and-drop of files *across* the screen edge (v1 uses an in-app drop zone)

### Terminology used in this doc

- **Node** — one machine running the app. Every node is peer-capable.
- **Active node** — the node whose physical mouse/keyboard is currently driving. Changes on handoff.
- **Origin node** — the node with the physical hardware you're touching. In v1 both nodes can be origin (bidirectional).
- **Handoff** — transfer of the active role from one node to the other.
- **Control channel** — the low-latency TCP connection carrying input/clipboard/handoff.
- **Bulk channel** — the separate TCP connection carrying file data.

---

## Tier 1 — Toolchain Setup

### 1.1 Version targets

| Tool | Version | Why pinned |
|---|---|---|
| Rust | 1.98+ stable, **edition 2024** | Edition 2024 is default since 1.85; 1.98 is current stable (Aug 2026) |
| Tauri | 2.11.x | Current v2 line as of mid-2026 |
| Node.js | 24 LTS | Needed for the frontend build step on both machines |
| pnpm | 10.x | Faster than npm, better monorepo handling |

Rust ships a new stable every 6 weeks. Pin the *minimum* in `Cargo.toml` via `rust-version = "1.98"` so you get a clear error rather than a confusing one if you ever build on an older toolchain.

---

### 1.2 macOS setup (your primary dev machine)

**Step 1 — Xcode Command Line Tools** (required; provides the linker and system headers)

```bash
xcode-select --install
```

Verify:
```bash
xcode-select -p
# should print: /Library/Developer/CommandLineTools
```

**Step 2 — Rust via rustup**

Never install Rust via Homebrew. `rustup` is the official toolchain manager and is what lets you switch versions, add targets, and install components.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Accept the default installation (option 1). Then restart your shell or:
```bash
source "$HOME/.cargo/env"
```

Verify:
```bash
rustc --version   # rustc 1.98.0 (or newer)
cargo --version
rustup --version
```

**Step 3 — Add components and targets**

```bash
# Components: formatter, linter, and the language server backend
rustup component add rustfmt clippy rust-src rust-analyzer

# Targets: build for both Apple Silicon and Intel Macs
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

> **Note on Windows targets from Mac:** do *not* bother adding `x86_64-pc-windows-msvc` here. You cannot link MSVC binaries from macOS without the Microsoft linker. You're building Windows natively on your Windows box, which is the correct approach for a project doing low-level input hooking anyway.

**Step 4 — Node.js + pnpm**

```bash
# Install fnm (fast node manager) — cleaner than nvm, works well on Apple Silicon
brew install fnm
echo 'eval "$(fnm env --use-on-cd --shell zsh)"' >> ~/.zshrc
source ~/.zshrc

fnm install 24
fnm default 24
node --version   # v24.x

# pnpm
corepack enable
corepack prepare pnpm@latest --activate
pnpm --version
```

**Step 5 — Tauri CLI**

Install as a Cargo tool so the version is tied to your Rust toolchain:

```bash
cargo install tauri-cli --version "^2.0" --locked
cargo tauri --version   # tauri-cli 2.11.x
```

The `--locked` flag makes it use the crate's committed `Cargo.lock`, which avoids surprise dependency breakage during install.

**Step 6 — Useful Cargo extras** (optional but genuinely helpful while learning)

```bash
cargo install cargo-watch    # auto-rebuild on file save
cargo install cargo-expand   # see what macros expand to — great for understanding
cargo install cargo-edit     # `cargo add`/`cargo rm` for dependency management
cargo install cargo-nextest  # much better test runner output
```

---

### 1.3 Windows setup

**Step 1 — Visual Studio Build Tools** (must come before Rust)

Rust on Windows uses the MSVC toolchain by default, which needs Microsoft's linker.

1. Download **Build Tools for Visual Studio 2022** from the Microsoft downloads page (search "Visual Studio Build Tools" — it's under "Tools for Visual Studio").
2. In the installer, select the **"Desktop development with C++"** workload.
3. In the right-hand panel, confirm these are checked:
   - MSVC v143 build tools (or newer)
   - Windows 11 SDK (latest version)
4. Install. This is a several-GB download; let it finish completely.

You do **not** need full Visual Studio. Build Tools is sufficient.

**Step 2 — WebView2 Runtime**

Tauri renders the UI in the system webview. On Windows 11 this is pre-installed. On Windows 10 or if unsure, install the **Evergreen Bootstrapper** from Microsoft's WebView2 page. Verify by checking for "Microsoft Edge WebView2 Runtime" in Installed Apps.

**Step 3 — Rust via rustup**

Download and run `rustup-init.exe` from `https://rustup.rs`. Choose the default (option 1) — this selects `x86_64-pc-windows-msvc`, which is what you want.

Open a **new** terminal (PowerShell or Windows Terminal) and verify:
```powershell
rustc --version
cargo --version
```

Then add components:
```powershell
rustup component add rustfmt clippy rust-src rust-analyzer
```

**Step 4 — Node.js + pnpm**

```powershell
# Install fnm via winget
winget install Schniz.fnm

# Add to your PowerShell profile so it loads each session
# Run: notepad $PROFILE   (create the file if prompted)
# Add this line:
#   fnm env --use-on-cd | Out-String | Invoke-Expression

fnm install 24
fnm default 24
node --version

corepack enable
corepack prepare pnpm@latest --activate
```

**Step 5 — Tauri CLI**

```powershell
cargo install tauri-cli --version "^2.0" --locked
cargo tauri --version
```

**Step 6 — Git**

```powershell
winget install Git.Git
```

Set line endings correctly so you don't get CRLF churn between machines:
```powershell
git config --global core.autocrlf input
```

On macOS:
```bash
git config --global core.autocrlf input
```

---

### 1.4 Toolchain pinning (do this once, commit it)

Create `rust-toolchain.toml` at the repo root. Both machines will then automatically use the same toolchain — rustup reads this file and installs/switches as needed.

```toml
[toolchain]
channel = "1.98"
components = ["rustfmt", "clippy", "rust-src", "rust-analyzer"]
targets = ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"]
profile = "default"
```

> This is the single most useful thing you can do for a two-machine project. It eliminates "works on my Mac" toolchain drift entirely.

---

## Tier 2 — IDE & Editor Setup

### Recommendation: VS Code on both machines

Consistency across machines matters more than marginal feature differences, and the Rust + Tauri + React combination is best served by one editor that handles all three.

**Extensions (identical on both machines):**

| Extension | Purpose |
|---|---|
| `rust-lang.rust-analyzer` | The Rust language server. Non-negotiable. |
| `tauri-apps.tauri-vscode` | Tauri config schema, command completion |
| `vadimcn.vscode-lldb` | Debugger (CodeLLDB) — works on both macOS and Windows |
| `tamasfe.even-better-toml` | `Cargo.toml` / `tauri.conf.json` editing |
| `usernamehw.errorlens` | Shows compiler errors inline — enormously helpful while learning Rust |
| `dbaeumer.vscode-eslint` | Frontend linting |
| `esbenp.prettier-vscode` | Frontend formatting |

**Workspace settings** — create `.vscode/settings.json` and commit it:

```json
{
  "rust-analyzer.check.command": "clippy",
  "rust-analyzer.check.extraArgs": ["--all-targets", "--all-features"],
  "rust-analyzer.cargo.features": "all",
  "rust-analyzer.inlayHints.typeHints.enable": true,
  "rust-analyzer.inlayHints.parameterHints.enable": true,
  "rust-analyzer.inlayHints.closureReturnTypeHints.enable": "always",
  "editor.formatOnSave": true,
  "[rust]": {
    "editor.defaultFormatter": "rust-lang.rust-analyzer"
  },
  "[typescript]": {
    "editor.defaultFormatter": "esbenp.prettier-vscode"
  },
  "[typescriptreact]": {
    "editor.defaultFormatter": "esbenp.prettier-vscode"
  },
  "files.watcherExclude": {
    "**/target/**": true
  }
}
```

> **Inlay hints are the single best learning aid for Rust.** They show you inferred types and lifetimes inline. Leave them on until types become second nature, then dial back.

**Alternative: RustRover (JetBrains)** — free for non-commercial use, better refactoring and debugging out of the box. Reasonable choice if you prefer JetBrains, but you'd then want VS Code (or WebStorm) alongside it for the React side. VS Code for everything is simpler.

---

## Tier 3 — Project Scaffolding

### 3.1 Create the project

On your Mac:

```bash
cd ~/projects   # or wherever you keep things
pnpm create tauri-app@latest
```

Answer the prompts:
```
Project name           → hoppr
Identifier             → com.yourname.hoppr
Frontend language      → TypeScript / JavaScript
Package manager        → pnpm
UI template            → React
UI flavor              → TypeScript
```

Then:
```bash
cd hoppr
pnpm install
pnpm tauri dev    # confirm the default window opens before going further
```

**Do not skip that verification step.** If the default app doesn't run, fix the toolchain before adding any complexity.

### 3.2 Restructure into a Cargo workspace

The generated project puts everything in `src-tauri`. You want the core logic in its own crate so it's testable without the GUI, and the platform code isolated. Restructure to:

```
hoppr/
├── Cargo.toml                    # workspace root
├── rust-toolchain.toml
├── .vscode/settings.json
├── README.md
│
├── crates/
│   ├── hoppr-core/               # ← pure logic, ZERO OS-specific code
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── config.rs         # settings load/save (TOML)
│   │       ├── topology.rs       # screen layout, edge math, coordinate scaling
│   │       ├── state.rs          # the handoff state machine
│   │       ├── remap.rs          # modifier remapping tables
│   │       ├── protocol/
│   │       │   ├── mod.rs
│   │       │   ├── messages.rs   # every wire message type
│   │       │   ├── codec.rs      # framing: length-prefix + postcard
│   │       │   └── version.rs    # protocol version negotiation
│   │       ├── net/
│   │       │   ├── mod.rs
│   │       │   ├── discovery.rs  # mDNS advertise + browse
│   │       │   ├── control.rs    # control channel: connect, read/write loop
│   │       │   ├── bulk.rs       # bulk channel for transfers
│   │       │   ├── pairing.rs    # 6-digit code exchange, cert pinning
│   │       │   └── tls.rs        # rustls setup
│   │       ├── transfer/
│   │       │   ├── mod.rs
│   │       │   ├── sender.rs
│   │       │   ├── receiver.rs
│   │       │   └── manifest.rs   # file metadata, chunk plan, resume state
│   │       └── traits.rs         # ← THE KEY FILE: platform abstraction
│   │
│   ├── hoppr-platform/           # ← ALL OS-specific code lives here
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs            # cfg-gated re-exports
│   │       ├── windows/
│   │       │   ├── mod.rs
│   │       │   ├── capture.rs    # SetWindowsHookEx + message pump
│   │       │   ├── inject.rs     # SendInput
│   │       │   ├── clipboard.rs  # AddClipboardFormatListener
│   │       │   ├── screens.rs    # EnumDisplayMonitors, DPI awareness
│   │       │   └── keycodes.rs   # VK codes ↔ our normalized enum
│   │       └── macos/
│   │           ├── mod.rs
│   │           ├── capture.rs    # CGEventTap
│   │           ├── inject.rs     # CGEventPost
│   │           ├── clipboard.rs  # NSPasteboard changeCount polling
│   │           ├── screens.rs    # NSScreen / CGDisplay
│   │           ├── keycodes.rs   # CGKeyCode ↔ our normalized enum
│   │           └── permissions.rs# Accessibility permission check + prompt
│   │
│   └── hoppr-app/                # ← Tauri binary (was src-tauri)
│       ├── Cargo.toml
│       ├── tauri.conf.json
│       ├── build.rs
│       ├── capabilities/
│       ├── icons/
│       └── src/
│           ├── main.rs
│           ├── commands.rs       # #[tauri::command] fns the UI calls
│           ├── events.rs         # events pushed core → UI
│           └── tray.rs           # system tray menu
│
└── ui/                           # ← React frontend (was src/)
    ├── package.json
    ├── vite.config.ts
    ├── tsconfig.json
    └── src/
        ├── main.tsx
        ├── App.tsx
        ├── lib/
        │   ├── ipc.ts            # typed wrappers around Tauri invoke/listen
        │   └── types.ts          # TS mirrors of Rust message types
        ├── components/
        │   ├── LayoutCanvas.tsx
        │   ├── MachineTile.tsx
        │   ├── ConnectionPanel.tsx
        │   ├── TransferPanel.tsx
        │   ├── TransferItem.tsx
        │   ├── RemapEditor.tsx
        │   ├── LogViewer.tsx
        │   └── StatusBar.tsx
        └── styles/
```

### 3.3 Root `Cargo.toml`

```toml
[workspace]
resolver = "3"                    # required for edition 2024
members = ["crates/*"]

# Shared dependency versions — individual crates use `workspace = true`
# so you only ever bump a version in one place.
[workspace.dependencies]
tokio        = { version = "1", features = ["full"] }
serde        = { version = "1", features = ["derive"] }
postcard     = { version = "1", features = ["alloc"] }
thiserror    = "2"
anyhow       = "1"
tracing      = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
blake3       = "1"
rustls       = "0.23"
tokio-rustls = "0.26"
rcgen        = "0.13"
mdns-sd      = "0.13"
arboard      = "3"
toml         = "0.8"
directories  = "6"

[workspace.package]
edition      = "2024"
rust-version = "1.98"

[profile.release]
opt-level     = 3
lto           = "fat"      # link-time optimization — smaller, faster binary
codegen-units = 1          # slower compile, better optimization
strip         = true       # strip symbols — significantly smaller binary
panic         = "abort"    # no unwinding machinery; smaller binary

[profile.dev]
opt-level = 1              # dev builds are painfully slow at opt-level 0 for
                           # input-hot-path code; 1 is a good compromise
```

> `resolver = "3"` and `edition = "2024"` go together. If you see resolver errors, this is why.

---

## Tier 4 — Rust Concepts You'll Actually Use

You don't need all of Rust. You need these, in roughly this order. Read *The Rust Book* chapters listed, but learn each one by applying it to this project.

### 4.1 Ownership & borrowing (Book ch. 4)
The one genuinely new idea. Every value has exactly one owner; when the owner goes out of scope, the value is dropped. You can lend out references (`&T` shared, `&mut T` exclusive) but never both at once.

**Where you'll hit it:** passing input events from the capture callback into the network sender. You'll want to `clone()` more than feels right at first. That's fine — a `MouseMove` event is 16 bytes; cloning it costs nothing. Optimize later, if ever.

### 4.2 Traits (Book ch. 10)
The abstraction mechanism. Like C# interfaces, but resolved at compile time by default (zero cost) and usable dynamically via `dyn Trait` when needed.

**Where you'll hit it:** `traits.rs` is the spine of this whole project. See [Tier 5.2](#52-the-platform-trait-boundary).

### 4.3 Enums & pattern matching (Book ch. 6, 18)
Rust enums carry data. This is how you model your protocol messages, and `match` forces you to handle every case.

```rust
// This is a "sum type" — a value is exactly ONE of these variants.
// The compiler will refuse to compile a `match` that misses a variant,
// which means adding a new message type gives you a compile error at every
// place that needs updating. This is a feature, not an annoyance.
pub enum ControlMessage {
    MouseMove { x: f32, y: f32 },
    KeyDown { code: KeyCode, mods: Modifiers },
    Handoff { target: NodeId, entry: EdgePoint },
    // ...
}
```

### 4.4 `Result` and `?` (Book ch. 9)
No exceptions. Fallible functions return `Result<T, E>`. The `?` operator propagates errors up.

**Convention for this project:**
- Library crates (`hoppr-core`, `hoppr-platform`) → `thiserror` for typed, matchable errors
- Binary crate (`hoppr-app`) → `anyhow` for easy context-adding

```rust
// In hoppr-core — a typed error the caller can match on:
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message exceeds max frame size: {0} bytes")]
    FrameTooLarge(usize),
    #[error("protocol version mismatch: peer speaks v{peer}, we speak v{ours}")]
    VersionMismatch { peer: u16, ours: u16 },
    #[error("decode failed: {0}")]
    Decode(#[from] postcard::Error),
}
```

### 4.5 `Arc`, `Mutex`, and channels (Book ch. 15–16)
- `Arc<T>` — shared ownership across threads (atomic refcount)
- `Mutex<T>` / `RwLock<T>` — shared *mutable* access
- `tokio::sync::mpsc` — async channels, your main tool for moving events between tasks

**Design rule for this project:** prefer channels over shared mutexes. Your OS capture callback runs on a hot path and must not block. It should do nothing but shove an event into an unbounded channel and return immediately.

```rust
// In the capture callback — the ONLY thing it does:
let _ = tx.send(event);   // non-blocking, never waits on a lock
```

### 4.6 `async` / `.await` and Tokio (Tokio tutorial)
Async is for I/O concurrency, not CPU parallelism. Your network loops, discovery, and file transfers are all async. Your input capture hooks are *not* — they're OS callbacks on dedicated OS threads, bridged into async via channels.

**The mental model that matters:** `spawn` a task per concern (control channel reader, control channel writer, bulk transfer, discovery, clipboard watcher). Let them talk over channels. Don't try to do everything in one loop.

### 4.7 `unsafe` and FFI (Book ch. 20)
You'll need it for the OS APIs. Rules for this project:
1. `unsafe` blocks appear **only** in `hoppr-platform`. Never in `hoppr-core`.
2. Every `unsafe` block gets a `// SAFETY:` comment explaining why it's sound.
3. Wrap each unsafe call in a safe function immediately. The unsafe surface should be small and local.

```rust
/// Injects a synthetic mouse move at absolute screen coordinates.
///
/// # Safety
/// `SendInput` requires the INPUT struct to be correctly sized and the
/// union variant to match the `type` field. We construct it fully here,
/// so both invariants hold. The call itself cannot fail in a way that
/// corrupts memory; it returns 0 on rejection, which we surface as an error.
pub fn move_cursor_absolute(x: i32, y: i32) -> Result<(), InjectError> {
    let input = build_mouse_input(x, y);
    // SAFETY: `input` is a fully-initialized INPUT with type == INPUT_MOUSE
    // and the `mi` union member populated, matching Win32's contract.
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent == 0 { return Err(InjectError::Rejected); }
    Ok(())
}
```

---

## Tier 5 — Architecture

### 5.1 Layer diagram

```
┌─────────────────────────────────────────────────────────┐
│  ui/  — React + TypeScript                              │
│  Settings, layout canvas, transfers, logs, status       │
└────────────────────────┬────────────────────────────────┘
                         │  Tauri IPC
                         │  invoke() commands ↓   emit() events ↑
┌────────────────────────┴────────────────────────────────┐
│  hoppr-app/  — Tauri binary                             │
│  Command handlers, event forwarding, tray, lifecycle    │
└────────────────────────┬────────────────────────────────┘
                         │  plain Rust calls + channels
┌────────────────────────┴────────────────────────────────┐
│  hoppr-core/  — portable logic (NO os-specific code)    │
│  State machine · topology · protocol · net · transfer   │
│  100% unit-testable without a real mouse or network     │
└────────────────────────┬────────────────────────────────┘
                         │  trait objects
┌────────────────────────┴────────────────────────────────┐
│  hoppr-platform/  — cfg-gated OS implementations        │
│    #[cfg(windows)]        #[cfg(target_os = "macos")]   │
│    Win32 hooks/SendInput  CGEventTap/CGEventPost        │
└─────────────────────────────────────────────────────────┘
```

**The rule that makes this work:** `hoppr-core` must compile and pass its full test suite on *any* platform, with zero `#[cfg]` attributes. If you find yourself writing `#[cfg(windows)]` in core, the abstraction is wrong — move it behind a trait.

---

### 5.2 The platform trait boundary

This is `crates/hoppr-core/src/traits.rs`. Write it first; everything else follows from it.

```rust
//! Platform abstraction boundary.
//!
//! Every OS-specific capability the app needs is expressed here as a trait.
//! `hoppr-core` depends only on these traits and never on a concrete OS API.
//! `hoppr-platform` provides one implementation per supported OS.
//!
//! This is what makes the core testable: tests substitute mock implementations
//! and exercise the full handoff state machine with no real hardware involved.

use crate::protocol::{InputEvent, KeyCode, Modifiers};

/// Captures global input events, optionally suppressing them from reaching
/// the local OS.
///
/// Implementations run the OS hook on a dedicated thread and forward events
/// through the channel supplied to `start`. The callback path must never
/// block — on Windows a slow low-level hook is silently unregistered by the
/// OS after the timeout, and on macOS a slow event tap gets disabled.
pub trait InputCapture: Send + 'static {
    /// Begins capturing. Events are pushed to `sink` as they arrive.
    fn start(&mut self, sink: tokio::sync::mpsc::UnboundedSender<InputEvent>)
        -> Result<(), PlatformError>;

    /// Stops capturing and releases the OS hook.
    fn stop(&mut self) -> Result<(), PlatformError>;

    /// When `true`, captured events are consumed and do NOT reach local apps.
    /// This is what makes the local cursor "disappear" during handoff.
    fn set_suppression(&mut self, suppress: bool) -> Result<(), PlatformError>;

    /// True if the OS revoked our hook (macOS does this after sleep, and after
    /// an event tap times out). The supervisor polls this to trigger re-arm.
    fn is_healthy(&self) -> bool;
}

/// Injects synthetic input into the local OS.
pub trait InputSink: Send + 'static {
    fn inject(&mut self, event: &InputEvent) -> Result<(), PlatformError>;

    /// Warps the cursor to absolute screen coordinates. Used on handoff entry.
    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), PlatformError>;

    /// Forces all modifier keys to the released state. Called on handoff exit
    /// and on disconnect — this is the fix for stuck-modifier bugs.
    fn release_all_modifiers(&mut self) -> Result<(), PlatformError>;
}

/// Watches and sets the system clipboard.
pub trait ClipboardProvider: Send + 'static {
    /// Emits an event whenever local clipboard content changes.
    fn watch(&mut self, sink: tokio::sync::mpsc::UnboundedSender<ClipboardEvent>)
        -> Result<(), PlatformError>;

    fn set_text(&mut self, text: &str) -> Result<(), PlatformError>;
    fn set_image(&mut self, png_bytes: &[u8]) -> Result<(), PlatformError>;
}

/// Reports the local display configuration.
pub trait ScreenInfo: Send + 'static {
    /// All displays attached to this machine, in OS virtual-desktop coords.
    fn displays(&self) -> Vec<Display>;

    /// The bounding box of the entire virtual desktop. Edge detection uses this.
    fn virtual_bounds(&self) -> Rect;

    /// DPI scale factor for a given display. Needed to make cursor position
    /// translate correctly between a Retina Mac and a 1080p Windows monitor.
    fn scale_factor(&self, display_id: DisplayId) -> f64;
}

/// OS-level permission gates (macOS Accessibility; no-op on Windows).
pub trait PermissionGate: Send + 'static {
    fn has_input_permission(&self) -> bool;
    /// Opens the relevant system settings pane. Returns immediately.
    fn request_input_permission(&self) -> Result<(), PlatformError>;
}
```

### 5.3 Dependency wiring

In `hoppr-platform/src/lib.rs`, expose a single factory so `hoppr-app` never writes a `#[cfg]`:

```rust
//! Platform implementations, selected at compile time.
//!
//! Callers use `current_platform()` and receive boxed trait objects. Which
//! concrete types they get is decided by `cfg` here and nowhere else.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(windows)]
use windows as imp;

pub struct Platform {
    pub capture:    Box<dyn InputCapture>,
    pub sink:       Box<dyn InputSink>,
    pub clipboard:  Box<dyn ClipboardProvider>,
    pub screens:    Box<dyn ScreenInfo>,
    pub permissions:Box<dyn PermissionGate>,
}

/// Builds the platform bundle for whichever OS this binary was compiled for.
pub fn current_platform() -> Result<Platform, PlatformError> {
    Ok(Platform {
        capture:     Box::new(imp::Capture::new()?),
        sink:        Box::new(imp::Sink::new()?),
        clipboard:   Box::new(imp::Clipboard::new()?),
        screens:     Box::new(imp::Screens::new()?),
        permissions: Box::new(imp::Permissions::new()),
    })
}
```

### 5.4 Which crates for the OS layer

**Windows** — use the official Microsoft-maintained `windows` crate:
```toml
[target.'cfg(windows)'.dependencies]
windows = { version = "0.60", features = [
    "Win32_Foundation",
    "Win32_UI_WindowsAndMessaging",   # SetWindowsHookEx, message pump
    "Win32_UI_Input_KeyboardAndMouse",# SendInput, VK codes
    "Win32_Graphics_Gdi",             # EnumDisplayMonitors
    "Win32_System_DataExchange",      # clipboard
    "Win32_System_LibraryLoader",
    "Win32_UI_HiDpi",                 # DPI awareness
]}
```

**macOS** — the `objc2` family is the maintained standard now (the older `cocoa` and `core-foundation` crates are effectively deprecated):
```toml
[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.6"
objc2-foundation = "0.3"
objc2-app-kit = "0.3"                 # NSScreen, NSPasteboard
objc2-core-graphics = "0.3"           # CGEventTap, CGEventPost
```

> **On `rdev` / `enigo`:** these are fine for a *quick prototype* to see events flowing, and worth an afternoon just to confirm your setup works. But they don't give you reliable event *suppression* (which a KVM absolutely requires — otherwise your local cursor keeps moving while you're driving the remote machine), and `rdev` is a lightly-maintained pet project with several competing forks. Plan to write the platform layer directly against `windows` and `objc2`. It's more work but it's the difference between "a KVM that mostly works" and the exceptional tool you described.

### 5.5 Threading model

```
Windows                              macOS
───────                              ─────
[hook thread]                        [tap thread]
  SetWindowsHookEx                     CGEventTap on kCGHIDEventTap
  GetMessage() pump                    CFRunLoop
  callback → tx.send()                 callback → tx.send()
        │                                    │
        └──────────── mpsc channel ──────────┘
                          │
              ┌───────────▼────────────┐
              │  Tokio runtime         │
              │  ┌──────────────────┐  │
              │  │ event router     │  │  ← decides local vs remote
              │  ├──────────────────┤  │
              │  │ control tx task  │  │  ← writes to control socket
              │  │ control rx task  │  │  ← reads, dispatches to sink
              │  ├──────────────────┤  │
              │  │ discovery task   │  │
              │  │ clipboard task   │  │
              │  │ transfer tasks   │  │  ← one per active transfer
              │  ├──────────────────┤  │
              │  │ supervisor task  │  │  ← health checks, reconnect,
              │  └──────────────────┘  │     re-arm hooks after sleep
              └────────────────────────┘
```

**Critical constraint:** the OS hook callback must return in **well under 1ms**. On Windows, a low-level hook that exceeds `LowLevelHooksTimeout` (default 300ms but effectively much tighter in practice) gets silently unregistered by the OS — your app just stops working with no error. On macOS, a slow event tap gets disabled and you must re-enable it on `kCGEventTapDisabledByTimeout`. Handle that event explicitly; it's not optional.

---

## Tier 6 — Wire Protocol Specification

### 6.1 Framing

Every message on both channels:

```
┌──────────────┬─────────────────────────┐
│ u32 LE length│  postcard-encoded body  │
│  (4 bytes)   │      (length bytes)     │
└──────────────┴─────────────────────────┘
```

- Length is the body size only, not including the 4-byte header.
- Max frame: **16 MB** on bulk, **1 MB** on control. Reject anything larger — this prevents a malformed length from causing a huge allocation.
- Use `tokio_util::codec::LengthDelimitedCodec` rather than hand-rolling this.

### 6.2 Why postcard

`postcard` is a compact, no-schema binary format built for embedded use. A `MouseMove` encodes to ~7 bytes vs ~40 for JSON. At 200 events/sec that difference is real, and more importantly the *decode* cost is near zero — no string parsing on your latency-critical path.

```toml
postcard = { version = "1", features = ["alloc"] }
```

### 6.3 Message types

```rust
//! Wire protocol message definitions.
//!
//! # Versioning rule
//! Adding a variant at the END of an enum is backward-compatible with
//! postcard (older peers will fail to decode it, so gate new variants behind
//! the negotiated protocol version). Reordering or removing variants is a
//! BREAKING change — bump PROTOCOL_VERSION.

pub const PROTOCOL_VERSION: u16 = 1;

// ─────────────────────────── Handshake ───────────────────────────

#[derive(Serialize, Deserialize, Debug)]
pub enum Handshake {
    /// First message on any new connection.
    Hello {
        protocol_version: u16,
        node_id: NodeId,            // stable UUID, generated on first run
        display_name: String,       // "Zach's MacBook"
        os: OsKind,
        app_version: String,
    },
    /// Response. `accepted: false` means version mismatch or unknown peer.
    HelloAck {
        protocol_version: u16,
        node_id: NodeId,
        display_name: String,
        os: OsKind,
        accepted: bool,
        reason: Option<String>,
    },
}

// ─────────────────────── Control channel ─────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ControlMessage {
    // --- Input (the hot path) ---

    /// Cursor position, normalized 0.0–1.0 within the target's virtual desktop.
    /// NORMALIZED, not pixels — this is what makes mismatched resolutions and
    /// DPI work correctly. The receiver multiplies by its own bounds.
    MouseMove { x: f32, y: f32 },

    /// Relative motion, for when the receiver has pointer acceleration or a
    /// game has captured the cursor. Sent alongside MouseMove when available.
    MouseDelta { dx: i16, dy: i16 },

    MouseDown  { button: MouseButton },
    MouseUp    { button: MouseButton },
    Scroll     { dx: i16, dy: i16, precise: bool },

    /// Key events carry a NORMALIZED physical key code, not a character.
    /// Remapping happens on the RECEIVING side, so each machine owns its
    /// own remap rules.
    KeyDown { code: KeyCode, repeat: bool },
    KeyUp   { code: KeyCode },

    /// Authoritative snapshot of which modifiers are physically held.
    /// MUST be sent immediately before every Handoff and every Reclaim.
    /// This is the single fix for the "everything is selecting" stuck-modifier
    /// class of bug that plagues Barrier and Synergy.
    ModifierState { mods: Modifiers },

    // --- Handoff ---

    /// "You have control now. Put your cursor here."
    /// `entry` is normalized position along the shared edge.
    Handoff { entry: EdgePoint },

    /// "I'm taking control back." Sent by the machine reclaiming.
    Reclaim,

    /// Sent by whichever node the escape hotkey was pressed on.
    /// Both nodes immediately release all modifiers and return control local.
    EmergencyRelease,

    // --- Clipboard ---

    /// Content is inline if small; otherwise it's offered and pulled over bulk.
    ClipboardUpdate {
        seq: u64,                   // monotonic; ignore out-of-order updates
        content: ClipboardContent,
    },

    // --- Transfer coordination (data itself goes over bulk) ---

    TransferOffer   { transfer_id: TransferId, manifest: FileManifest },
    TransferAccept  { transfer_id: TransferId, resume_from: u64 },
    TransferReject  { transfer_id: TransferId, reason: String },
    TransferCancel  { transfer_id: TransferId },
    TransferComplete{ transfer_id: TransferId, hash: [u8; 32] },

    // --- Housekeeping ---

    /// Sent every 2s. Doubles as the latency measurement.
    Ping { seq: u64, sent_at_micros: u64 },
    Pong { seq: u64, sent_at_micros: u64 },

    /// Peer's screen layout changed (monitor plugged in, resolution change).
    ScreenConfig { displays: Vec<Display>, virtual_bounds: Rect },

    /// Graceful shutdown notice.
    Goodbye { reason: String },
}

// ───────────────────────── Bulk channel ──────────────────────────

#[derive(Serialize, Deserialize, Debug)]
pub enum BulkMessage {
    /// One chunk of file data. `offset` is the byte position in the file,
    /// which is what makes resume work.
    Chunk {
        transfer_id: TransferId,
        offset: u64,
        data: Vec<u8>,          // 512 KiB default
    },
    /// Large clipboard payload (image) that didn't fit inline.
    ClipboardBlob {
        seq: u64,
        mime: String,
        data: Vec<u8>,
    },
}

// ───────────────────────── Supporting types ──────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl:  bool,
    pub alt:   bool,     // Option on Mac
    pub meta:  bool,     // Cmd on Mac, Win key on Windows
    pub caps:  bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct EdgePoint {
    pub edge: Edge,       // which edge of the receiver we're entering from
    pub pos:  f32,        // 0.0–1.0 along that edge
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileManifest {
    pub name: String,
    pub size: u64,
    pub hash: [u8; 32],   // BLAKE3 of full file
    pub chunk_size: u32,
    pub modified: Option<u64>,  // unix seconds, preserved on receive
}
```

### 6.4 Socket options (do not skip these)

```rust
// Control channel — latency is everything, throughput is irrelevant.
control_socket.set_nodelay(true)?;   // disable Nagle: send tiny packets NOW

// Bulk channel — throughput matters, latency does not.
bulk_socket.set_nodelay(false)?;     // let Nagle coalesce
// Larger buffers help saturate gigabit:
bulk_socket.set_send_buffer_size(1 << 21)?;   // 2 MiB
```

> Forgetting `TCP_NODELAY` on the control channel is the #1 cause of "why does the mouse feel laggy." Nagle's algorithm will happily hold your 7-byte mouse event for 40ms waiting for more data.

### 6.5 Ports

| Purpose | Port | Notes |
|---|---|---|
| Control | 24800 | Configurable. 24800 is Synergy's, so it's a familiar default |
| Bulk | 24801 | Control port + 1 |
| mDNS | 5353 | Standard, don't change |

Service type for discovery: `_hoppr._tcp.local.`

---

## Tier 7 — Feature Specifications

### 7.1 Handoff state machine

The core of the app. Implemented in `hoppr-core/src/state.rs`, fully unit-testable.

```rust
//! Handoff state machine.
//!
//! Every input event and network message is fed through `handle()`, which
//! returns a list of Actions for the caller to execute. The state machine
//! itself performs NO I/O — this is what makes it testable.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    /// Not connected to a peer. All input passes through locally.
    Disconnected,

    /// Connected; local machine has control. Input goes to the local OS.
    /// We watch cursor position for edge crossings.
    LocalActive,

    /// Connected; the REMOTE machine has control. Our input is captured and
    /// suppressed locally, then forwarded over the wire.
    RemoteActive,

    /// Connected; the remote is driving US. We inject what they send.
    /// Our own local input (if any) is ignored while in this state.
    BeingDriven,

    /// Edge handoff temporarily disabled by the user (lock-to-screen).
    Locked,
}
```

**Transitions:**

| From | Trigger | To | Actions |
|---|---|---|---|
| `Disconnected` | peer handshake OK | `LocalActive` | start heartbeat, sync clipboard |
| `LocalActive` | cursor crosses configured edge | `RemoteActive` | send `ModifierState`, send `Handoff`, enable suppression, hide + **decouple** local cursor |
| `RemoteActive` | receives `ReleaseBack` | `LocalActive` | disable suppression, warp local cursor to remembered position, `release_all_modifiers()` |
| `RemoteActive` | escape hotkey | `LocalActive` | send `EmergencyRelease`, disable suppression, `release_all_modifiers()` |
| `*` | receives `Handoff` | `BeingDriven` | `warp_cursor()` to entry point, arm back-out detector, begin injecting |
| `BeingDriven` | driven cursor pushed back out the shared edge | `LocalActive` | send `ReleaseBack`, `release_all_modifiers()` |
| `BeingDriven` | receives `Reclaim` | `LocalActive` | `release_all_modifiers()`, restore local cursor |
| `*` | connection lost | `Disconnected` | disable suppression, `release_all_modifiers()`, start reconnect backoff |
| `LocalActive` | lock toggle | `Locked` | ignore edge crossings |

**Reclaim is decided on the machine BEING driven, not by the driver.** The original design had the driver watch its own cursor cross back over the edge; in practice the driver's cursor is suppressed (and on macOS keeps physically moving unless decoupled with `CGAssociateMouseAndMouseCursorPosition(false)`), so that check fired on ordinary jitter and the boundary "kept re-grabbing." Instead: the driven side integrates the motion it's injecting, and once the cursor is pushed back out through the edge it entered on it sends `ReleaseBack`. The `RemoteActive → LocalActive` on the driver is a *response* to that message. `Reclaim` (driver-initiated) is kept in the protocol and state machine for a future explicit "take control back" affordance, but nothing sends it today. A short inward-travel arm step after entry keeps the very first jitter sample from reading as an immediate exit.

**Non-negotiable invariant:** any transition out of `RemoteActive` or `BeingDriven` calls `release_all_modifiers()` on the affected machine. Write this as a test.

### 7.2 Edge detection & coordinate math

`hoppr-core/src/topology.rs`.

**Layout model:** each node has a normalized rectangle on a shared abstract canvas. Two nodes share an edge when their rectangles are adjacent.

```rust
/// Converts a cursor position on the LOCAL virtual desktop into a normalized
/// entry point on the REMOTE machine's shared edge.
///
/// Normalization is the whole trick: we never send pixels. A cursor at 60%
/// down the right edge of a 1440p Mac enters at 60% down the left edge of a
/// 1080p Windows box, regardless of DPI or resolution differences.
pub fn compute_entry_point(
    local_bounds: Rect,
    cursor: Point,
    edge: Edge,
) -> EdgePoint {
    let pos = match edge {
        Edge::Right | Edge::Left =>
            (cursor.y - local_bounds.y) as f32 / local_bounds.height as f32,
        Edge::Top | Edge::Bottom =>
            (cursor.x - local_bounds.x) as f32 / local_bounds.width as f32,
    };
    EdgePoint { edge: edge.opposite(), pos: pos.clamp(0.0, 1.0) }
}
```

**Edge trigger rules:**
- Trigger when cursor reaches within **1 pixel** of the boundary AND is still moving in that direction (check the delta sign — this prevents accidental triggers from stopping at the edge).
- Add a configurable **lock delay** (default 0ms, but useful at 150–250ms for people who overshoot).
- After handoff, apply a **200ms cooldown** before the reverse handoff can fire. Without this you get flicker at the boundary.
- Optional **corner dead zones** (default 20px) so hitting the corner for a UI element doesn't trigger a handoff.

### 7.3 Modifier remapping

`hoppr-core/src/remap.rs`. Applied on the **receiving** side, at injection time.

```rust
/// A remap table maps physical key codes to what should actually be injected.
/// Stored per-machine in config, so your Mac can swap Ctrl↔Cmd while your
/// Windows box leaves everything alone.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct RemapTable {
    /// Physical code → injected code.
    pub rules: HashMap<KeyCode, KeyCode>,

    /// Invert scroll direction. Mac "natural scrolling" vs Windows means
    /// forwarded scroll events feel backwards without this.
    pub invert_scroll_y: bool,
    pub invert_scroll_x: bool,
}

impl RemapTable {
    /// Your case: a Windows keyboard driving a Mac.
    /// Ctrl→Cmd means Ctrl+C/V/A/Z/Tab all work as muscle memory expects.
    pub fn windows_keyboard_on_mac() -> Self {
        Self {
            rules: HashMap::from([
                (KeyCode::LeftCtrl,  KeyCode::LeftMeta),
                (KeyCode::RightCtrl, KeyCode::RightMeta),
                (KeyCode::LeftMeta,  KeyCode::LeftCtrl),
                (KeyCode::RightMeta, KeyCode::RightCtrl),
            ]),
            invert_scroll_y: true,   // if the Mac has natural scrolling on
            invert_scroll_x: false,
        }
    }
}
```

**Edge cases to test explicitly:**
- `Cmd+Space` (Spotlight) — must reach the Mac, not be swallowed
- `Cmd+Tab` vs `Alt+Tab` — decide whether these are handled locally or forwarded (recommend: forwarded, so you're switching apps on the machine you're driving)
- `Cmd+Shift+4` (screenshot) — needs the remap applied to *all three* keys consistently
- Holding a modifier *across* a handoff — covered by `ModifierState` snapshots
- Caps Lock — it's a toggle, not a hold. Track state, don't forward press/release naively.

### 7.4 Clipboard sync

**macOS:** `NSPasteboard` has no change notification. Poll `changeCount` every 250ms; it's a cheap integer read. Only fetch content when the count changes.

**Windows:** use `AddClipboardFormatListener` and handle `WM_CLIPBOARDUPDATE` — proper event-driven, no polling.

**Rules:**
- Inline text under **256 KB** in the `ClipboardUpdate` control message.
- Anything larger (images especially) → send an offer, transfer over the **bulk** channel. Never put a 5 MB PNG on the control channel; it will stall your mouse.
- Hard cap at **10 MB** by default, configurable. Above that, skip sync and log it.
- Use a monotonic `seq` counter to prevent echo loops (A sets clipboard → B receives and sets → B's watcher fires → B sends back to A → infinite loop). When you set the clipboard from a remote update, record the resulting local `changeCount` and ignore the next watcher fire that matches it.
- `arboard` crate handles the cross-platform read/write; you write the watching yourself.

### 7.5 File transfer

**Performance targets on gigabit ethernet:** ≥110 MB/s sustained (near line rate). If you're getting 30 MB/s, something in this list is wrong.

| Parameter | Value | Rationale |
|---|---|---|
| Chunk size | 512 KiB | Balance between syscall overhead and progress granularity |
| Compression | **Off by default** | At 1 Gbps, gzip is the bottleneck, not the wire |
| Hashing | BLAKE3 | Hashes faster than gigabit; SHA-256 would bottleneck you |
| Buffering | Reused `Vec<u8>` | Never allocate per chunk |
| Read path | `tokio::fs::File` + `read_exact` | Or `tokio::io::copy_buf` for the simple case |
| In-flight chunks | Up to 4 | Pipelining hides round-trip latency |

**Flow:**
```
Sender                                  Receiver
──────                                  ────────
hash file (BLAKE3, streaming)
build FileManifest
  ──── TransferOffer ─────────────────▶
                                        check policy:
                                          Ask / AlwaysAccept / AlwaysDeny
                                        check for partial file → resume_from
  ◀─── TransferAccept{resume_from} ────
open file, seek to resume_from
  ──── Chunk{offset, data} ───────────▶  write at offset
  ──── Chunk{offset, data} ───────────▶  update progress
  ──── ...                    ────────▶
  ──── TransferComplete{hash} ────────▶  verify BLAKE3
                                        rename .part → final name
                                        restore mtime
```

**Resume state:** write a sidecar `.hoppr-part` JSON next to the incoming file recording `{transfer_id, expected_hash, bytes_received}`. On a new offer with a matching hash, resume from there.

**Accept policy** (per the toggles you specified):
```rust
#[derive(Serialize, Deserialize, Clone, Copy)]
pub enum AcceptPolicy {
    Ask,          // prompt every time (default)
    AlwaysAccept, // auto-accept from this paired peer
    AlwaysDeny,   // silently reject, log it
}
```
Store per-peer, not globally. Surface it in the UI as a dropdown on the transfer panel *and* as a checkbox in the accept prompt ("always accept from this device").

### 7.6 Discovery & pairing

**Discovery** — `mdns-sd` crate:
```rust
// Advertise: _hoppr._tcp.local., with TXT records carrying
//   node_id, display_name, os, protocol_version, control_port
// Browse: same service type, filter out our own node_id.
```
Show discovered peers in the connection panel with a one-click Connect. Manual IP entry stays available as a fallback (some networks block mDNS).

**Pairing** — first connection only:
1. Both nodes generate a self-signed cert on first run (`rcgen`), stored in the app data dir.
2. On first connect, both display the same **6-digit code** derived from a hash of both certs' fingerprints (so a MITM can't produce a matching code).
3. User confirms on both sides.
4. Each node stores the other's cert fingerprint. Subsequent connections verify against the pinned fingerprint — no code needed.
5. A changed fingerprint → hard fail with a clear warning, not a silent reconnect.

**TLS:** `rustls` + `tokio-rustls`, with a custom certificate verifier that checks against your pinned fingerprint rather than a CA chain. Both channels are TLS. You are sending raw keystrokes — including passwords — over the network. This is not optional even on a trusted LAN.

### 7.7 Reliability features

**Reconnect:** exponential backoff — 500ms, 1s, 2s, 4s, 8s, capped at 30s. Reset on successful connect. Never give up entirely; keep retrying at the cap.

**Heartbeat:** `Ping`/`Pong` every 2s. Three missed pongs (6s) → declare dead, transition to `Disconnected`, run the safety actions.

**Sleep/wake:**
- **macOS:** register for `NSWorkspaceDidWakeNotification`. On wake, tear down and rebuild the event tap — the old one is invalid. Also handle `kCGEventTapDisabledByTimeout` and `kCGEventTapDisabledByUserInput` in the tap callback by re-enabling with `CGEventTapEnable`.
- **Windows:** handle `WM_POWERBROADCAST` / `PBT_APMRESUMEAUTOMATIC`. Re-register hooks.
- **Both:** the supervisor task polls `is_healthy()` every 5s as a backstop. If unhealthy, tear down and re-arm.

**Escape hotkey:** default `Ctrl+Alt+Shift+Escape` (unlikely to collide). Registered as a *global* hotkey on both machines, always active, even in `RemoteActive`. Triggers `EmergencyRelease`. This must work when everything else is broken, so implement it with its own direct path — do not route it through the normal state machine queue.

**Cursor position memory:** store the last cursor position per node. On reclaim, warp back there rather than leaving it at the edge. Small detail, noticeably better feel.

---

## Tier 8 — Frontend Specification

### 8.1 Panels

**1. Connection**
- Role: this version is peer-to-peer, so no server/client toggle is strictly needed — but expose "Accept incoming connections" as a toggle if you want a machine to be listen-only
- Device name (editable, persisted)
- Discovered devices list (from mDNS) with Connect buttons — **primary path**
- Manual IP + port entry — **fallback, visually secondary**
- Pairing code display/confirm modal
- Paired devices list with Forget option

**2. Layout**
- Canvas with draggable machine tiles
- Tiles sized to real aspect ratio, labeled with name + resolution
- Snap-to-edge when dragged near another tile; highlight the shared edge in accent color
- Settings per edge: enabled, lock delay (ms), corner dead zone (px)

**3. Input**
- Remap table editor: rows of `[physical key] → [injected key]`, add/remove
- Preset button: "Windows keyboard on Mac (swap Ctrl/Cmd)"
- Scroll direction toggles (invert X, invert Y)
- Escape hotkey binding (click-to-record)
- Lock-to-screen toggle (also expose as a tray item)

**4. Transfers**
- Drop zone — large, obvious, accepts multi-file
- Active transfers: name, progress bar, speed (rolling 1s average, not instantaneous), ETA, transferred/total, cancel button
- Accept policy dropdown per paired device
- History list with "Reveal in Finder/Explorer"

**5. Logs**
- Level filter (Error / Warn / Info / Debug / Trace)
- Search/filter box
- Copy-all button
- Auto-scroll toggle
- Open log file location button

**6. Status bar** (always visible, bottom)
- Connection state dot + peer name
- Latency in ms (from Ping/Pong)
- Which machine currently has control
- Lock indicator when locked

### 8.2 IPC contract

Keep this typed and centralized. `ui/src/lib/ipc.ts`:

```typescript
// Every Tauri command the frontend can call, in one place with real types.
// If a Rust signature changes, this file is where TypeScript will complain.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { Config, PeerInfo, TransferState, LogEntry, StatusSnapshot } from './types';

// ── Commands (UI → Rust) ──
export const getConfig    = () => invoke<Config>('get_config');
export const saveConfig   = (c: Config) => invoke<void>('save_config', { config: c });
export const listPeers    = () => invoke<PeerInfo[]>('list_discovered_peers');
export const connectPeer  = (addr: string) => invoke<void>('connect_peer', { addr });
export const disconnect   = () => invoke<void>('disconnect');
export const confirmPair  = (code: string) => invoke<boolean>('confirm_pairing', { code });
export const sendFiles    = (paths: string[]) => invoke<void>('send_files', { paths });
export const respondXfer  = (id: string, accept: boolean, always: boolean) =>
                              invoke<void>('respond_transfer', { id, accept, always });
export const setLocked    = (locked: boolean) => invoke<void>('set_locked', { locked });
export const checkPerms   = () => invoke<boolean>('has_input_permission');
export const requestPerms = () => invoke<void>('request_input_permission');

// ── Events (Rust → UI) ──
export const onStatus   = (cb: (s: StatusSnapshot) => void) =>
  listen<StatusSnapshot>('status:update', e => cb(e.payload));

export const onTransfer = (cb: (t: TransferState) => void) =>
  listen<TransferState>('transfer:update', e => cb(e.payload));

export const onLog      = (cb: (l: LogEntry) => void) =>
  listen<LogEntry>('log:entry', e => cb(e.payload));

export const onPairing  = (cb: (code: string) => void) =>
  listen<string>('pairing:code', e => cb(e.payload));
```

**Keeping Rust and TS types in sync:** use the `ts-rs` crate. Annotate your Rust types with `#[derive(TS)] #[ts(export)]` and it generates matching `.ts` definitions at test time. Run `cargo test` and the TypeScript types regenerate. This eliminates an entire category of bug.

### 8.3 Event throttling

Do **not** emit a Tauri event per input event — you'd flood the IPC bridge at 200+/sec and tank performance. Throttle UI updates:
- Status/latency: every 500ms
- Transfer progress: every 100ms (or every 1% — whichever is less frequent)
- Logs: batch and flush every 250ms

### 8.4 Design direction

You asked for "simple yet elegant." Concretely:
- **Dark by default**, with system-preference detection. This is a background utility; it should feel like a system tool.
- **One accent color** used sparingly — active connection, current edge, progress fill. Everything else neutral grays.
- **System font stack** (`-apple-system, 'Segoe UI Variable', system-ui`) — makes it feel native on both platforms rather than like a webpage.
- **The layout canvas is the hero.** It should be the first thing you see and the largest element. Everything else is secondary.
- Generous whitespace, no borders where a background shade will do, subtle transitions (150ms) on state changes.
- Tailwind CSS v4 for styling — it's fast to iterate with and you already know the idiom.

---

## Tier 9 — Logging & Commenting Standards

You said you want to understand how things work. This section is the mechanism for that. Follow it strictly for the first month; it will pay for itself.

### 9.1 Logging with `tracing`

Use `tracing`, not `log` or `println!`. It gives structured fields and spans, which matter enormously when debugging a distributed, concurrent system.

```rust
// Setup in main.rs
fn init_logging() -> Result<()> {
    use tracing_subscriber::{fmt, EnvFilter, prelude::*};

    // Log to BOTH a rolling file and the in-app viewer.
    let file_appender = tracing_appender::rolling::daily(log_dir(), "hoppr.log");

    tracing_subscriber::registry()
        // RUST_LOG env var overrides; default shows our crates at debug,
        // everything else at warn (so tokio internals don't drown you).
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_|
            EnvFilter::new("warn,hoppr_core=debug,hoppr_platform=debug,hoppr_app=debug")))
        .with(fmt::layer().with_writer(file_appender).json())
        .with(fmt::layer().with_writer(std::io::stdout).pretty())
        .with(UiLayer::new(app_handle))   // custom layer → Tauri events
        .init();
    Ok(())
}
```

**Level discipline:**

| Level | Use for | Example |
|---|---|---|
| `error!` | Something broke and the user needs to know | Hook registration failed, cert mismatch |
| `warn!` | Degraded but recovering | Reconnect attempt, clipboard over size cap |
| `info!` | Notable state changes | Connected to peer, handoff occurred, transfer complete |
| `debug!` | Flow detail while developing | Message sent/received (types, not payloads), state transitions |
| `trace!` | Per-event firehose | Individual mouse moves. **Never on by default.** |

**Never log at `info` or above on the input hot path.** A `debug!` per mouse move is 200 log lines/sec and will itself cause the latency you're trying to debug.

**Use spans for multi-step operations:**
```rust
#[tracing::instrument(skip(file), fields(transfer_id = %id, size = manifest.size))]
async fn send_file(id: TransferId, manifest: FileManifest, file: File) -> Result<()> {
    info!("starting transfer");
    // Every log inside this fn automatically carries transfer_id and size.
    // When two transfers run concurrently, you can still tell them apart.
}
```

### 9.2 Commenting standard

Since Rust is new to you, comment more than you normally would — you are writing notes to yourself.

**Every module** gets a `//!` doc comment explaining its role and any non-obvious constraint:
```rust
//! Windows input capture via low-level hooks.
//!
//! `SetWindowsHookEx(WH_MOUSE_LL, ...)` requires a thread with a running
//! message pump, so this module spawns a dedicated OS thread and runs
//! `GetMessage` on it. The hook callback is invoked ON THAT THREAD by the OS.
//!
//! CRITICAL: the callback must return in well under the system
//! `LowLevelHooksTimeout`. If it exceeds it, Windows silently unregisters
//! the hook with no error and the app stops working. We therefore do nothing
//! in the callback except forward to a channel.
```

**Every public item** gets a `///` doc comment covering what, why, and gotchas:
```rust
/// Normalizes a physical key code from the OS into our platform-independent
/// `KeyCode` enum.
///
/// # Why normalize
/// Windows VK codes and macOS CGKeyCodes disagree on almost everything.
/// Sending a raw VK code to a Mac would inject garbage. We convert to a
/// shared enum on capture and back to the native code on injection.
///
/// # Returns
/// `None` for keys we don't model (media keys, vendor-specific keys). The
/// caller should drop these rather than guessing.
pub fn vk_to_keycode(vk: u16) -> Option<KeyCode> { ... }
```

**Inline `//` comments** for anything that would make you say "wait, why?" in three months. Especially:
- Magic numbers (always explain the unit and where it came from)
- Anything that looks wrong but is correct
- Workarounds for OS quirks — always link the reason

```rust
// macOS delivers scroll deltas ~10x larger than Windows for the same physical
// wheel click. Scale down so a click feels the same on both machines.
// Empirically derived; adjust if it feels off with a different mouse.
const MACOS_SCROLL_SCALE: f32 = 0.1;
```

**Every `unsafe` block** gets a `// SAFETY:` comment. No exceptions. Clippy will enforce this if you enable the lint.

### 9.3 Lints — put this at the top of each `lib.rs`

```rust
#![warn(missing_docs)]                    // every public item documented
#![warn(clippy::pedantic)]                // aggressive lints — great teacher
#![warn(clippy::undocumented_unsafe_blocks)]
#![allow(clippy::module_name_repetitions)]// too noisy, not useful
```

Clippy pedantic will feel harsh at first. Read each suggestion — it's the single fastest way to learn idiomatic Rust. Turn individual lints off if a rule genuinely doesn't fit, but read it before you silence it.

---

## Tier 10 — Build, Run, Release

### 10.1 Daily development (macOS)

```bash
# Full app with hot-reloading frontend + auto-rebuilt Rust
pnpm tauri dev

# Core logic only — much faster loop when you're not touching UI
cargo test -p hoppr-core
cargo watch -x 'test -p hoppr-core'

# Check everything compiles without producing binaries (fastest feedback)
cargo check --workspace --all-targets

# Lint
cargo clippy --workspace --all-targets -- -D warnings

# Format
cargo fmt --all
```

### 10.2 Daily development (Windows)

Identical commands. That's the point of pinning the toolchain.

```powershell
pnpm tauri dev
cargo test -p hoppr-core
cargo clippy --workspace --all-targets -- -D warnings
```

### 10.3 Release builds

**macOS** (universal binary covering Apple Silicon + Intel):
```bash
pnpm tauri build --target universal-apple-darwin
# Output: crates/hoppr-app/target/universal-apple-darwin/release/bundle/
#   ├── macos/hoppr.app
#   └── dmg/hoppr_0.1.0_universal.dmg
```

**Windows:**
```powershell
pnpm tauri build
# Output: crates\hoppr-app\target\release\bundle\
#   ├── msi\hoppr_0.1.0_x64_en-US.msi
#   └── nsis\hoppr_0.1.0_x64-setup.exe
```

### 10.4 Cross-machine workflow (before you learn CI)

```
Mac                                    Windows
───                                    ───────
work on feature branch
cargo test -p hoppr-core   ✓
pnpm tauri dev             ✓ (Mac side works)
git push
                                       git pull
                                       cargo test -p hoppr-core   ✓
                                       pnpm tauri dev             ✓
                                       ← test the actual two-machine handoff
                                       report issues back
```

Test the *real* two-machine flow at least once per feature. Single-machine testing will not catch handoff bugs — those only appear with two real OS event systems interacting.

**Loopback testing trick:** you can run two instances on the *same* machine on different ports with a fake second display in the topology. This catches protocol and state machine bugs quickly without switching chairs. It will *not* catch platform capture/injection bugs.

### 10.5 Binary size expectations

With the release profile in Tier 3.3, expect roughly:
- macOS `.app`: 8–14 MB
- Windows installer: 6–10 MB
- Idle RAM: 25–45 MB (most of it the webview; the Rust daemon itself is ~5 MB)

If you're seeing 100 MB+, check that `strip = true` and `lto = "fat"` are actually applied and you're building `--release`.

---

## Tier 11 — OS Permissions & Signing

### 11.1 macOS Accessibility permission

**This is where most Mac utilities feel broken. Get it right.**

Your app needs Accessibility permission to create an event tap. Without it, `CGEventTapCreate` returns null and — critically — **macOS gives no error and no prompt**. The app just silently doesn't work.

**Required flow:**
1. On startup, call `AXIsProcessTrusted()`. 
2. If false, show a **blocking, explanatory screen** in the UI — not a toast, not a log line. Explain what's needed and why.
3. Provide a button that calls `AXIsProcessTrustedWithOptions` with `kAXTrustedCheckOptionPrompt: true`, which triggers the system prompt.
4. Also provide a "Open System Settings" button that opens the pane directly:
   ```
   x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility
   ```
5. **Poll `AXIsProcessTrusted()` every 1s while on that screen** and advance automatically when granted. Do not make the user restart the app or click "check again."

**Development gotcha:** during `tauri dev`, the permission attaches to the *dev binary*, and its path changes on rebuild — so you'll be re-granting permission constantly. Two mitigations: grant permission to your terminal app (covers `cargo run` children), and do permission-sensitive testing against a built `.app` rather than the dev binary.

Also add to `Info.plist` via `tauri.conf.json`:
```json
{
  "bundle": {
    "macOS": {
      "entitlements": null,
      "files": {},
      "minimumSystemVersion": "11.0"
    }
  }
}
```

### 11.2 macOS Input Monitoring

Separate from Accessibility on modern macOS. Some event tap configurations also require **Input Monitoring** (`kTCCServiceListenEvent`). Check for it and guide the user to:
```
x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent
```

### 11.3 Windows

No permission dialogs needed for `SetWindowsHookEx` at the same integrity level. Two caveats:

1. **UIPI (User Interface Privilege Isolation):** a non-elevated process cannot inject input into an elevated window. If you run something as Administrator, your KVM will stop working over that window specifically. Options: run the app elevated (adds a UAC prompt at startup — annoying), or accept the limitation and document it. **Recommend: accept it.** Note it in the UI.

2. **Windows Defender / SmartScreen:** an unsigned installer that registers keyboard hooks will trip SmartScreen and may get flagged heuristically. For personal use, click through it. If you ever distribute this, you'd need code signing.

3. **DPI awareness:** set per-monitor DPI awareness v2 in the manifest, or your coordinate math will be silently wrong on scaled displays.

### 11.4 Signing (personal use)

For two machines you own, skip it:
- **macOS:** unsigned `.app` → right-click → Open the first time, or `xattr -cr /Applications/hoppr.app` to clear quarantine.
- **Windows:** unsigned installer → SmartScreen "More info" → "Run anyway."

If you later want this to be genuinely shareable, you'd need an Apple Developer account ($99/yr) for notarization and an Authenticode cert for Windows. Not worth it now.

---

## Tier 12 — Testing Strategy

The architecture exists to make this possible. Use it.

### 12.1 Unit tests — `hoppr-core` (the bulk of your tests)

Because core has no OS dependencies, you can test everything with mocks:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The stuck-modifier bug is the single most common failure mode in
    /// tools like this. Test it explicitly, at every exit path.
    #[test]
    fn handoff_with_held_modifier_releases_on_both_sides() {
        let mut sm = StateMachine::new(test_topology());
        sm.force_state(State::LocalActive);

        // User holds Ctrl, then slides across the edge.
        sm.handle(Input::KeyDown(KeyCode::LeftCtrl));
        let actions = sm.handle(Input::CursorAt(Point { x: 1919, y: 500 }));

        // Modifier snapshot MUST precede the handoff message.
        assert!(matches!(actions[0], Action::Send(ControlMessage::ModifierState { .. })));
        assert!(matches!(actions[1], Action::Send(ControlMessage::Handoff { .. })));

        // And the local machine must let go of Ctrl.
        assert!(actions.iter().any(|a| matches!(a, Action::ReleaseAllModifiers)));
    }

    #[test]
    fn connection_loss_always_releases_modifiers() {
        for state in [State::RemoteActive, State::BeingDriven] {
            let mut sm = StateMachine::new(test_topology());
            sm.force_state(state);
            let actions = sm.handle(Input::ConnectionLost);
            assert!(actions.contains(&Action::ReleaseAllModifiers),
                    "state {state:?} failed to release modifiers on disconnect");
            assert!(actions.contains(&Action::SetSuppression(false)));
        }
    }

    #[test]
    fn entry_point_scales_across_mismatched_resolutions() {
        // 60% down a 1440p right edge → 60% down a 1080p left edge.
        let ep = compute_entry_point(
            Rect { x: 0, y: 0, width: 2560, height: 1440 },
            Point { x: 2559, y: 864 },   // 864/1440 = 0.6
            Edge::Right,
        );
        assert_eq!(ep.edge, Edge::Left);
        assert!((ep.pos - 0.6).abs() < 0.001);
    }
}
```

### 12.2 Property tests

`proptest` for the coordinate math — this is where subtle bugs hide:
```rust
proptest! {
    /// Round-tripping a cursor position through normalize→denormalize must
    /// land within a pixel, for ANY pair of screen sizes.
    #[test]
    fn edge_point_roundtrips(
        w1 in 800u32..8000, h1 in 600u32..5000,
        w2 in 800u32..8000, h2 in 600u32..5000,
        y  in 0u32..5000,
    ) {
        prop_assume!(y < h1);
        // ... assert round-trip within 1px
    }
}
```

### 12.3 Integration tests

Two in-process nodes over loopback, no real hardware:
```rust
#[tokio::test]
async fn full_handoff_over_loopback() {
    let (node_a, mock_a) = spawn_test_node(MockPlatform::new()).await;
    let (node_b, mock_b) = spawn_test_node(MockPlatform::new()).await;
    node_a.connect(node_b.addr()).await.unwrap();

    mock_a.simulate_cursor_to_edge(Edge::Right);

    // B should have received a warp to the mirrored entry point.
    let injected = mock_b.injected_events().await;
    assert!(matches!(injected[0], InputEvent::WarpCursor { .. }));
}
```

### 12.4 Manual test checklist (run before each milestone is "done")

- [ ] Handoff left→right and right→left, repeatedly, no flicker at the boundary
- [ ] Hold Shift, cross the edge, release — no stuck modifier on either machine
- [ ] Ctrl+C on Windows → Cmd+V on Mac (remap working end to end)
- [ ] Cmd+Space reaches Mac Spotlight while being driven
- [ ] Copy 5 MB image, verify it arrives and doesn't stall the cursor
- [ ] Transfer a 2 GB file; confirm ≥100 MB/s and that the cursor stays smooth throughout
- [ ] Kill the network mid-transfer, reconnect, confirm resume works
- [ ] Sleep the Mac, wake it, confirm capture re-arms without restarting the app
- [ ] Unplug ethernet, replug, confirm auto-reconnect
- [ ] Escape hotkey returns control from a wedged state
- [ ] Lock-to-screen prevents edge handoff
- [ ] Change display resolution while connected; layout updates and math stays correct

---

## Tier 13 — Milestone Roadmap

Each milestone is independently demoable. Don't start the next until the current one is solid — this is a project where compounding half-working layers becomes unfixable.

### M0 — Skeleton *(~1 weekend)*
- Toolchain on both machines, `rust-toolchain.toml` committed
- Workspace structure created, all crates compile empty
- Tauri window opens on both machines
- `tracing` set up, logs to file and stdout
- **Demo:** empty app runs on Mac and Windows

### M1 — Platform traits + Windows capture *(~1 week)*
- `traits.rs` written and finalized
- Windows `InputCapture` via `SetWindowsHookEx` on a dedicated pump thread
- Windows `InputSink` via `SendInput`
- Windows `ScreenInfo`
- **Demo:** log every mouse/key event to the console; inject a synthetic click and see it land

### M2 — Core state machine *(~1 week)*
- `topology.rs` with edge math + full test coverage
- `state.rs` with all transitions + tests including the stuck-modifier cases
- Zero I/O in either — pure logic
- **Demo:** `cargo test -p hoppr-core` green, meaningful test count

### M3 — Protocol + local networking *(~1 week)*
- Message types, postcard codec, length framing
- Control channel connect/read/write over plain TCP (no TLS yet)
- Handshake and version negotiation
- Ping/Pong with latency measurement
- **Demo:** two instances on one machine exchange handshakes and heartbeats

### M4 — First real handoff, Windows→Windows *(~1 week)*
- Wire capture → state machine → network → injection
- Suppression working (local cursor stops when remote is active)
- Escape hotkey
- **Demo:** if you have two Windows machines, real handoff. Otherwise two instances with a simulated second display.

### M5 — macOS platform layer *(~2 weeks — this is the hardest milestone)*
- `CGEventTap` capture with suppression
- `CGEventPost` injection
- Accessibility permission detection + guided flow
- macOS `ScreenInfo` with correct Retina scale factors
- Keycode mapping table, tested against real keys
- **Demo:** real Mac ↔ Windows handoff. This is the moment it becomes a real product.

### M6 — Modifier remapping *(~3 days)*
- Remap table, config persistence
- Ctrl↔Cmd preset
- Scroll inversion
- Modifier state snapshots on every handoff
- **Demo:** Ctrl+C on your Windows keyboard copies on the Mac

### M7 — Clipboard sync *(~4 days)*
- Watchers on both platforms
- Text inline, images over bulk
- Echo-loop prevention
- Size cap
- **Demo:** copy on one machine, paste on the other, both text and images

### M8 — Security *(~4 days)*
- Self-signed cert generation, `rustls` on both channels
- 6-digit pairing flow
- Cert pinning + mismatch handling
- **Demo:** connection refuses to establish with an unpaired peer

### M9 — Discovery *(~2 days)*
- mDNS advertise + browse
- Discovered devices in UI
- **Demo:** open the app on both machines, they find each other with no IP entry

### M10 — File transfer *(~1 week)*
- Bulk channel, chunked send/receive
- BLAKE3 verification, resume, accept policies
- Progress reporting
- **Demo:** 2 GB file at line rate with a live progress bar

### M11 — UI build-out *(~2 weeks)*
- All six panels
- Layout canvas with drag + snap
- Log viewer
- Status bar
- Visual polish pass
- **Demo:** it looks like real software

### M12 — Reliability hardening *(~1 week)*
- Reconnect with backoff
- Sleep/wake recovery on both platforms
- Health supervisor
- Full manual checklist passing
- **Demo:** leave it running for a week of actual work without touching it

**Realistic total: 3–4 months of evenings and weekends**, given Rust is new. M5 is where you should expect to get genuinely stuck for a while — budget for it and don't be discouraged.

---

## Tier 14 — Troubleshooting Reference

| Symptom | Likely cause | Fix |
|---|---|---|
| Mouse feels laggy / stuttery | Nagle's algorithm | `set_nodelay(true)` on the control channel |
| Windows hook silently stops working | Callback exceeded `LowLevelHooksTimeout` | Do nothing in the callback but `tx.send()` |
| macOS: no events at all, no error | Missing Accessibility permission | Check `AXIsProcessTrusted()`, guide the user |
| macOS: worked, then stopped after sleep | Event tap invalidated | Handle wake notification, rebuild the tap |
| macOS: stops under heavy load | `kCGEventTapDisabledByTimeout` | Handle that event type, call `CGEventTapEnable` again |
| Modifiers stuck down | Missing `ModifierState` on handoff | Send snapshot before every handoff; release on every exit |
| Cursor jumps to wrong spot on remote | Sending pixels instead of normalized coords | Normalize to 0.0–1.0 at the edge |
| Coordinates wrong on scaled displays | DPI unawareness | Per-monitor DPI awareness v2 on Windows; NSScreen `backingScaleFactor` on Mac |
| Handoff flickers at the boundary | No cooldown after handoff | 200ms cooldown before reverse handoff can fire |
| Clipboard loops infinitely | Echo from your own set | Track `changeCount` after setting; ignore the matching fire |
| File transfer slow (~30 MB/s) | Compression on, or chunks too small, or no pipelining | Disable compression, 512 KiB chunks, 4 in flight |
| Cursor stutters during file transfer | Bulk data on the control channel | Separate connections; that's the whole point of the split |
| `cargo build` fails on Windows with linker error | Missing MSVC Build Tools | Install "Desktop development with C++" workload |
| `pnpm tauri dev` fails on Windows | Missing WebView2 | Install the Evergreen Bootstrapper |
| Different behavior between machines | Toolchain drift | Confirm `rust-toolchain.toml` is committed and both machines respect it |
| Huge binary | Not building release, or profile not applied | `--release`, confirm `strip`/`lto` in root `Cargo.toml` |

---

## Tier 15 — Deferred / Future Work

### Third machine
Design decisions that keep this cheap later:
- `NodeId` is already a UUID, not a bool or an index
- Topology is a *graph* of node rectangles, not a hardcoded left/right pair
- The state machine already tracks "which node is active" rather than "am I active"
- Connections are already stored in a map keyed by `NodeId`, not a single `Option<Connection>`

Get all four of those right in v1 and adding node three is mostly UI work plus routing input to the correct peer.

### GitHub Actions CI
Once you want to stop manually building on Windows: a matrix workflow on `macos-latest` + `windows-latest` runners that runs `cargo test`, `cargo clippy`, and `pnpm tauri build`, uploading installers as artifacts. Worth doing around M8. Ask when you get there — it's about 40 lines of YAML and a few gotchas around caching.

### Ideas worth considering later
- **Cross-network relay** — a small Axum service coordinating NAT traversal so this works home ↔ office
- **Drag files across the screen edge** — dragging a file off the right edge starts a transfer. Much harder than the drop zone (requires OS drag-session interception) but it's the feature that would make this genuinely better than anything else out there.
- **Per-app remap profiles** — different modifier rules when a specific app is focused
- **Auto-updater** — Tauri has a built-in updater plugin; needs a signing key and a hosted manifest
- **Screen edge "peek"** — brief visual indicator on the target machine showing where the cursor will enter
- **Wake-on-LAN** — wake the sleeping machine when you push the cursor at its edge

---

## Appendix A — Reference Links

| Resource | URL |
|---|---|
| The Rust Book | https://doc.rust-lang.org/book/ |
| Rust by Example | https://doc.rust-lang.org/rust-by-example/ |
| Tokio Tutorial | https://tokio.rs/tokio/tutorial |
| Tauri v2 Docs | https://v2.tauri.app/ |
| `windows` crate docs | https://docs.rs/windows/ |
| `objc2` crate docs | https://docs.rs/objc2/ |
| Win32 SetWindowsHookEx | https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowshookexw |
| Quartz Event Services | https://developer.apple.com/documentation/coregraphics/quartz_event_services |
| Barrier source (reference) | https://github.com/debauchee/barrier |

## Appendix B — Quick Command Reference

```bash
# ── Setup verification (run on both machines) ──
rustc --version && cargo --version && node --version && cargo tauri --version

# ── Daily ──
pnpm tauri dev                                     # run the app
cargo check --workspace --all-targets              # fastest compile feedback
cargo test -p hoppr-core                           # core tests only
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all

# ── Debugging ──
RUST_LOG=hoppr_core=trace pnpm tauri dev           # firehose logging
RUST_LOG=hoppr_core::net=debug pnpm tauri dev      # one module only

# ── Release ──
pnpm tauri build                                   # native target
pnpm tauri build --target universal-apple-darwin   # Mac universal

# ── Maintenance ──
rustup update                                      # update toolchain
cargo update                                       # update deps within semver
cargo tree -d                                      # find duplicate deps
```

---

*Document version 1.0 — September 2026*
