# ccomidi-nih

Rust prototype of the ccomidi plugin using nih-plug for the plugin framework and Vizia for the editor.

This workspace is split into two crates:

- `plugin/`: the actual CLAP plugin (`ccomidi-nih`)
- `xtask/`: build and install helpers layered on top of `nih_plug_xtask`

## Workspace layout

```text
ccomidi-nih/
├── Cargo.toml           # workspace manifest
├── bundler.toml         # plugin bundle metadata for nih_plug_xtask
├── plugin/
│   ├── Cargo.toml       # plugin crate manifest
│   └── src/
│       ├── lib.rs       # plugin entry point and audio-thread integration
│       ├── editor.rs    # Vizia UI and editor-side app state
│       ├── params.rs    # nih-plug parameter definitions
│       ├── voicegroup.rs# JSON sidecar loading for poryaaaa state
│       └── core/
│           ├── mod.rs   # core module re-exports
│           ├── command.rs
│           ├── encode.rs
│           └── sender.rs
└── xtask/
    ├── Cargo.toml
    └── src/main.rs      # bundle/install commands
```

Generated build output goes in `target/`. The bundled CLAP artifact ends up in `target/bundled/ccomidi-nih.clap`.

## How the code is organized

### `plugin/src/lib.rs`

`lib.rs` is the plugin's root and the place where the framework-specific pieces are wired together.

Its responsibilities are:

- define `CComidiPlugin`, the nih-plug `Plugin` implementation
- own the shared parameter block (`Arc<CComidiParams>`)
- own `SenderCore`, the framework-agnostic MIDI emission engine
- bridge audio-thread host callbacks into the core sender
- retarget incoming MIDI events onto the selected output channel
- expose the editor by calling into `editor::create()`

If you want to understand the runtime flow first, start here.

### `plugin/src/params.rs`

`params.rs` defines the full host-visible parameter model with `#[derive(Params)]`.

This file is the source of truth for:

- the global channel/program parameters
- the fixed rows shown in the UI
- the dynamic rows and their assignable commands
- persisted editor window state (`ViziaState`)

The parameter structs are intentionally separate from the MIDI logic. Each audio block, `lib.rs` copies the current parameter values into `SenderCore`.

### `plugin/src/editor.rs`

`editor.rs` builds the Vizia interface.

It handles:

- font registration and UI styling
- layout of transport, fixed-command, and voicegroup sections
- custom UI interactions that write nih-plug parameters
- editor-only state that should not live in the host parameter model

The editor reads and writes parameters through Vizia lenses, while short-lived UI state such as the loaded instrument list lives in the local `Data` model.

### `plugin/src/core/`

This directory is the framework-agnostic MIDI engine. It contains the logic you would want to keep even if the plugin framework or UI toolkit changed.

#### `command.rs`

Defines:

- row and field count constants
- the `CommandType` enum
- fixed-row command mapping

This is the static description of what a row can represent.

#### `encode.rs`

Contains the pure encoding layer:

- `encode_row()` converts a row's command + fields into a short sequence of CC messages
- `EncodedCommand` is a fixed-capacity, allocation-free buffer used on the audio thread

This file is intentionally stateless and easy to unit test.

#### `sender.rs`

Contains `SenderCore`, the stateful runtime engine.

It is responsible for:

- tracking the current row/program/channel configuration
- detecting transport start
- deciding when a full snapshot must be emitted
- diffing against last-emitted state to avoid redundant MIDI output
- writing events through the abstract `EventSink` trait

`SenderCore` does not know about nih-plug, DAWs, or Vizia.

### `plugin/src/voicegroup.rs`

This module loads `poryaaaa_state.json`, validates it, and turns it into typed Rust data used by the editor.

It also contains:

- state-file path resolution logic
- `CCOMIDI_STATE_PATH` override support
- constants for the 14-bit Add-Instrument index transport

This code is editor-facing I/O, not audio-thread MIDI logic.

### `xtask/src/main.rs`

This crate exists so the workspace can use `cargo xtask ...` for packaging and install flows.

There are two main paths:

- `cargo xtask bundle ccomidi-nih --release`: build the CLAP bundle into `target/bundled/`
- `cargo xtask install ccomidi-nih`: bundle and then symlink the result into the user's CLAP directory

On macOS, install targets `~/Library/Audio/Plug-Ins/CLAP`.

## Runtime data flow

At a high level, the project is layered like this:

1. The host/DAW owns the plugin instance and calls nih-plug entry points.
2. `CComidiPlugin` reads the current parameter values from `CComidiParams`.
3. Those values are copied into `core::SenderCore`.
4. `SenderCore` decides what MIDI events should be emitted for this block.
5. `lib.rs` forwards those events into the host through nih-plug's `ProcessContext`.
6. Separately, the Vizia editor reads/writes parameters and loads voicegroup JSON for the UI.

That split is deliberate:

- `params.rs` and `editor.rs` are framework/UI-facing
- `core/` is logic-facing and testable in isolation
- `lib.rs` is the adapter layer between the two

## Building

From the workspace root:

```bash
cargo build -p ccomidi-nih
```

Build a release CLAP bundle:

```bash
cargo xtask bundle ccomidi-nih --release
```

Build and install it into your user CLAP folder:

```bash
cargo xtask install ccomidi-nih
```

On macOS, to build a universal binary:

```bash
cargo xtask install ccomidi-nih --universal
```

## Where to look when changing things

- Add or remove host-visible controls: `plugin/src/params.rs`
- Change how MIDI rows encode: `plugin/src/core/encode.rs`
- Change when MIDI is emitted: `plugin/src/core/sender.rs`
- Change pass-through or audio-thread behavior: `plugin/src/lib.rs`
- Change UI layout or editor-only behavior: `plugin/src/editor.rs`
- Change voicegroup JSON loading/path resolution: `plugin/src/voicegroup.rs`
- Change bundle/install flow: `xtask/src/main.rs`