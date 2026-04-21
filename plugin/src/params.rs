//! `nih-plug` parameter definitions.
//!
//! Layout (99 total params):
//!   - `channel`             — global MIDI output channel (0..=15)
//!   - `program_enabled` + `program` — optional program-change emission
//!   - `fixed_rows[4]`       — Volume / Pan / Mod / LfoSpeed, each with
//!                             `enabled` + single value (0..=127)
//!   - `dyn_rows[12]`        — freely-assigned rows, each with `enabled`,
//!                             a command-type picker, and four 0..=127 fields
//!
//! At audio time, `CComidiPlugin::sync_params_to_core` copies these values
//! into a [`crate::core::SenderCore`] which owns the actual emission logic.
//!
//! # Rust patterns in this file
//!
//! - `#[derive(Params)]` is provided by nih-plug and walks the struct at
//!   compile time to register every parameter with the host. Each leaf has
//!   a short `#[id = "…"]`; `#[nested]` recurses into sub-structs.
//! - `#[nested(array, group = "…")]` on a `[T; N]` expands to N nested
//!   sub-Params with auto-generated ids like `r0_en`, `r0_v`, …. No macro
//!   loop in user code.
//! - `AssignableCommand` is its own enum (a subset of `core::CommandType`)
//!   because only the plugin layer can derive nih-plug's `Enum` trait —
//!   keeping `core` framework-agnostic is worth the two-enum duplication.

use nih_plug::prelude::*;
use nih_plug_vizia::ViziaState;
use std::sync::Arc;

use crate::core::CommandType;

/// What a dynamic row can be assigned from the UI.
///
/// Order here is what the dropdown shows; `None` first so an enabled-but-
/// unconfigured row stays silent by default.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Enum)]
pub enum AssignableCommand {
    #[name = "None"]
    None,
    #[name = "Bend Range (CC 0x14)"]
    BendRange,
    #[name = "Mod Type (CC 0x16)"]
    ModType,
    #[name = "Tune (CC 0x18)"]
    Tune,
    #[name = "LFO Delay (CC 0x1A)"]
    LfoDelay,
    #[name = "Priority (CC 0x21)"]
    Priority21,
    #[name = "Priority (CC 0x27)"]
    Priority27,
    #[name = "xcmd iecv"]
    XcmdIecv,
    #[name = "xcmd iecl"]
    XcmdIecl,
    #[name = "MemAcc 0C"]
    MemAcc0C,
    #[name = "MemAcc 10"]
    MemAcc10,
}

/// Map the UI-facing enum back to the framework-agnostic core variant.
///
/// `impl From` is Rust's idiomatic conversion trait. `.into()` on a value
/// of type `AssignableCommand` at a place that wants a `CommandType` will
/// dispatch here automatically.
impl From<AssignableCommand> for CommandType {
    fn from(a: AssignableCommand) -> Self {
        match a {
            AssignableCommand::None => CommandType::None,
            AssignableCommand::BendRange => CommandType::BendRange,
            AssignableCommand::ModType => CommandType::ModType,
            AssignableCommand::Tune => CommandType::Tune,
            AssignableCommand::LfoDelay => CommandType::LfoDelay,
            AssignableCommand::Priority21 => CommandType::Priority21,
            AssignableCommand::Priority27 => CommandType::Priority27,
            AssignableCommand::XcmdIecv => CommandType::XcmdIecv,
            AssignableCommand::XcmdIecl => CommandType::XcmdIecl,
            AssignableCommand::MemAcc0C => CommandType::MemAcc0C,
            AssignableCommand::MemAcc10 => CommandType::MemAcc10,
        }
    }
}

// -----------------------------------------------------------------------------
// Per-row sub-Params
// -----------------------------------------------------------------------------

/// Per-row params for the four fixed commands (Volume, Pan, Mod, LfoSpeed).
///
/// Only one value field — these commands each emit a single CC byte that
/// takes field[0] as its data byte, so presenting four field sliders would
/// only confuse the user.
#[derive(Params)]
pub struct FixedRowParams {
    // Leaf ids are prefixed `fx` so they don't collide with `DynamicRowParams`
    // when both are used via `#[nested(array, …)]` on the top-level struct —
    // the array form auto-prefixes by index only, not by field name.
    #[id = "fxen"]
    pub enabled: BoolParam,
    #[id = "fxv"]
    pub value: IntParam,
}

impl FixedRowParams {
    fn new(label: &str, default_enabled: bool, default_value: i32) -> Self {
        Self {
            enabled: BoolParam::new(label, default_enabled),
            value: IntParam::new(
                format!("{label} Value"),
                default_value,
                IntRange::Linear { min: 0, max: 127 },
            ),
        }
    }
}

/// Per-row params for the 12 assignable dynamic rows.
///
/// All 4 fields are exposed because the longest command (`MemAcc*`) uses
/// all four. Commands that use fewer simply ignore the extras.
#[derive(Params)]
pub struct DynamicRowParams {
    // See note on `FixedRowParams` — ids are prefixed `dy` to stay unique
    // across both arrays.
    #[id = "dyen"]
    pub enabled: BoolParam,
    #[id = "dyc"]
    pub cmd: EnumParam<AssignableCommand>,
    #[id = "dyf0"]
    pub f0: IntParam,
    #[id = "dyf1"]
    pub f1: IntParam,
    #[id = "dyf2"]
    pub f2: IntParam,
    #[id = "dyf3"]
    pub f3: IntParam,
}

impl DynamicRowParams {
    fn new(row_idx: usize) -> Self {
        let label = format!("Row {row_idx}");
        // Shorthand: each field gets the same range, just with a numeric
        // suffix on the display name.
        let field = |n: u8| {
            IntParam::new(
                format!("{label} F{n}"),
                0,
                IntRange::Linear { min: 0, max: 127 },
            )
        };
        Self {
            // Short display-name so the button fits: just "Row N".
            enabled: BoolParam::new(&label, false),
            cmd: EnumParam::new(format!("{label} Cmd"), AssignableCommand::None),
            f0: field(0),
            f1: field(1),
            f2: field(2),
            f3: field(3),
        }
    }
}

// -----------------------------------------------------------------------------
// Top-level params struct
// -----------------------------------------------------------------------------

/// Fixed-row array length — kept as a literal in the type signature because
/// `#[derive(Params)]` runs before const evaluation. `FIXED_ROW_COUNT` from
/// `core` is enforced equal by the `static_assertions` below.
pub const FIXED_ROWS: usize = 6;
/// Dynamic-row array length (`MAX_ROWS - FIXED_ROW_COUNT`).
pub const DYN_ROWS: usize = 10;

// Compile-time sanity: if someone ever changes the core constants, this
// refuses to compile rather than silently going out of sync.
const _: () = assert!(FIXED_ROWS == crate::core::FIXED_ROW_COUNT);
const _: () = assert!(FIXED_ROWS + DYN_ROWS == crate::core::MAX_ROWS);

#[derive(Params)]
pub struct CComidiParams {
    /// Persist-only: Vizia's window size/position between plugin reloads.
    #[persist = "editor-state"]
    pub editor_state: Arc<ViziaState>,

    #[id = "ch"]
    pub channel: IntParam,

    #[id = "pe"]
    pub program_enabled: BoolParam,

    #[id = "p"]
    pub program: IntParam,

    /// 14-bit index for the Add-Instrument CC#98/#99 pair. Restored after
    /// an in-progress refactor temporarily removed it; still referenced
    /// by `editor.rs::add_instrument_row`.
    #[id = "ai"]
    pub add_instrument_index: IntParam,

    /// 4 fixed rows. Cross-array id collisions are prevented at the inner
    /// `#[id]` level: FixedRowParams uses `fxen`/`fxv`, DynamicRowParams
    /// uses `dyen`/`dyc`/…, so the resulting fully-qualified ids (e.g.
    /// `0_fxen`, `0_dyen`) stay unique.
    #[nested(array, group = "Fixed")]
    pub fixed_rows: [FixedRowParams; FIXED_ROWS],

    /// 12 dynamic rows.
    #[nested(array, group = "Rows")]
    pub dyn_rows: [DynamicRowParams; DYN_ROWS],
}

impl Default for CComidiParams {
    fn default() -> Self {
        Self {
            editor_state: crate::editor::default_state(),

            channel: IntParam::new("Channel", 0, IntRange::Linear { min: 0, max: 15 }),

            // Default on — the sender plugin's job is to drive a synth,
            // and most hosts expect Program Change to fire on transport
            // start. The user can still toggle it off.
            program_enabled: BoolParam::new("Program On", true),
            program: IntParam::new("Program", 0, IntRange::Linear { min: 0, max: 127 }),

            add_instrument_index: IntParam::new(
                "Add Instrument Index",
                0,
                IntRange::Linear {
                    min: 0,
                    max: crate::voicegroup::MAX_INSTRUMENT_INDEX as i32,
                },
            ),

            // Volume and Pan default enabled at 64 (center/half) so a
            // freshly-inserted plugin makes audible sound without having
            // to click two toggles. Mod / LFO Speed / xCIEV / xCIEL stay
            // off because their effect varies heavily per voicegroup.
            fixed_rows: [
                FixedRowParams::new("Volume", true, 64),
                FixedRowParams::new("Pan", true, 64),
                FixedRowParams::new("Mod", false, 0),
                FixedRowParams::new("LFO Speed", false, 0),
                FixedRowParams::new("xCIEV", false, 0),
                FixedRowParams::new("xCIEL", false, 0),
            ],

            // `std::array::from_fn` takes a `fn(usize) -> T` and produces a
            // `[T; N]` with no heap allocation. Safer than manual `[T; 12]`
            // literal because it stays correct if DYN_ROWS changes.
            dyn_rows: std::array::from_fn(|i| DynamicRowParams::new(FIXED_ROWS + i)),
        }
    }
}
