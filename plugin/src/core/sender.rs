//! [`SenderCore`] — the stateful engine that tracks the current row
//! configuration and emits CC / Program-Change events at the right times.
//!
//! Maps to the `SenderCore` class in `src/core/sender_core.{h,cpp}` in the
//! original C++ project.
//!
//! # What's implemented in this pass
//!
//! - State (channel, program, 16 rows, program-enable flag) and all setters
//! - Runtime state for detecting the transport play-start edge
//! - Full **snapshot** emission: when playback starts, emit the program
//!   change (if enabled) followed by every enabled row's CC sequence
//! - [`EventSink`] trait so the same logic drives both tests and the plugin
//!
//! # What's deferred to a later pass
//!
//! - Diff-on-parameter-change emission (the C++ `apply_parameter_change` +
//!   `emit_preapplied_changes` path). That requires hooking into the host's
//!   automation event stream, which is most idiomatic once we wire this into
//!   `nih-plug`'s `Plugin::process`. For now the only emission path is the
//!   transport-edge snapshot.
//! - Serialization. Easy to add later; state is already a plain tree of
//!   `Copy` types, so `serde` will derive it directly.

use super::command::{CommandType, FIXED_ROW_COUNT, MAX_FIELDS, MAX_ROWS};
use super::encode::{encode_row, EncodedCommand};

/// Callers of [`SenderCore`] implement this trait to receive the MIDI events
/// the core wants to emit.
///
/// # Why a trait?
///
/// The C++ version writes events into a caller-provided `PlannedEvents`
/// struct. The Rust equivalent is a trait because:
///   - it's zero-overhead (monomorphized when called with `impl EventSink`
///     — the compiler inlines the sink impl at each call site, same codegen
///     as a direct function call),
///   - it lets tests use a `Vec<Emission>` as a sink without the production
///     code taking a `Vec` dependency,
///   - it lets the real plugin wrap `nih-plug`'s `ProcessContext` without
///     SenderCore ever importing `nih-plug`.
pub trait EventSink {
    /// Emit one MIDI CC. `timing` is the sample offset within the current
    /// audio block (0 = first sample of the block).
    fn push_cc(&mut self, timing: u32, channel: u8, cc: u8, value: u8);

    /// Emit one MIDI Program Change.
    fn push_program(&mut self, timing: u32, channel: u8, program: u8);
}

/// Everything the user can configure for one row.
///
/// # Serialization boundary
///
/// This is the unit that maps to a block of the on-disk state. Keep it small
/// and `Copy` so we can snapshot the whole `[RowState; 16]` array cheaply.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RowState {
    pub enabled: bool,
    pub cmd: CommandType,
    pub fields: [u8; MAX_FIELDS],
}

impl Default for RowState {
    /// Rows default to disabled with `CommandType::None` and all fields
    /// zeroed. The fixed-row CommandType is *not* stored here — it's derived
    /// from the row index (see [`SenderCore::resolved_cmd_for_row`]).
    fn default() -> Self {
        Self {
            enabled: false,
            cmd: CommandType::None,
            fields: [0; MAX_FIELDS],
        }
    }
}

/// The engine.
///
/// # Invariants
///
/// - `channel` is always in `0..=15`.
/// - `program` is always in `0..=127`.
/// - Each `fields[i]` is always in `0..=127`.
///
/// Setters enforce these via [`clamp_*`](clamp_u8_127) — we never hand out a
/// mutable reference to raw state.
#[derive(Debug, Clone)]
pub struct SenderCore {
    // ---- User-facing configuration (persisted across plugin reload) ----
    channel: u8,
    program: u8,
    program_enabled: bool,
    rows: [RowState; MAX_ROWS],

    // ---- Runtime tracking (NOT persisted) ------------------------------
    // These exist only to decide "should we emit a snapshot right now?"
    // and to let a future diff-emission pass skip redundant CC writes.
    last_transport_playing: bool,
    last_emitted_channel: u8,
    last_emitted_program: Option<u8>,
    /// Last CC sequence we emitted per row. `None` means "never emitted" or
    /// "last emission was empty" — both are treated as "a re-emit will
    /// differ" by the future diff path.
    last_emitted_rows: [Option<EncodedCommand>; MAX_ROWS],
}

impl Default for SenderCore {
    fn default() -> Self {
        Self::new()
    }
}

impl SenderCore {
    /// Construct a fresh core in the default state: channel 0, program 0,
    /// program-change disabled, all 16 rows disabled.
    pub fn new() -> Self {
        Self {
            channel: 0,
            program: 0,
            program_enabled: false,
            rows: [RowState::default(); MAX_ROWS],
            last_transport_playing: false,
            last_emitted_channel: 0,
            last_emitted_program: None,
            last_emitted_rows: [None; MAX_ROWS],
        }
    }

    /// Clear everything to defaults. Equivalent to dropping and recreating
    /// but mutates in place so pointers / `Arc`s stay valid.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Clear only the *runtime* tracking — the user-visible config is
    /// preserved. Call this when the plugin is (re-)activated by the host
    /// so we don't think we "already emitted" things from a stale run.
    pub fn reset_runtime(&mut self) {
        self.last_transport_playing = false;
        self.last_emitted_program = None;
        self.last_emitted_rows = [None; MAX_ROWS];
        // `last_emitted_channel` is harmless stale; resynchronized at next
        // snapshot.
    }

    // ---- Setters --------------------------------------------------------
    //
    // These mirror the C++ `set_*` methods but accept already-quantized
    // integers instead of raw doubles — we'll do quantization at the
    // nih-plug param boundary rather than inside the core.

    pub fn set_channel(&mut self, channel: u8) {
        self.channel = channel.min(15);
    }

    pub fn set_program(&mut self, program: u8) {
        self.program = clamp_u8_127(program);
    }

    pub fn set_program_enabled(&mut self, enabled: bool) {
        self.program_enabled = enabled;
    }

    pub fn set_row_enabled(&mut self, row: usize, enabled: bool) {
        if let Some(r) = self.rows.get_mut(row) {
            r.enabled = enabled;
        }
    }

    /// Change what a *dynamic* row emits. No-op for fixed rows (0..4) — those
    /// always emit their hardcoded CommandType, matching the C++ behavior.
    pub fn set_row_cmd(&mut self, row: usize, cmd: CommandType) {
        if row < FIXED_ROW_COUNT {
            return;
        }
        if let Some(r) = self.rows.get_mut(row) {
            r.cmd = cmd;
        }
    }

    pub fn set_row_field(&mut self, row: usize, field: usize, value: u8) {
        if let Some(r) = self.rows.get_mut(row) {
            if field < MAX_FIELDS {
                r.fields[field] = clamp_u8_127(value);
            }
        }
    }

    // ---- Getters --------------------------------------------------------
    pub fn channel(&self) -> u8 {
        self.channel
    }
    pub fn program(&self) -> u8 {
        self.program
    }
    pub fn program_enabled(&self) -> bool {
        self.program_enabled
    }
    pub fn row(&self, row: usize) -> Option<&RowState> {
        self.rows.get(row)
    }
    /// What this row will actually emit — takes fixed-row override into
    /// account.
    pub fn resolved_cmd_for_row(&self, row: usize) -> CommandType {
        CommandType::fixed_for_row(row).unwrap_or_else(|| {
            self.rows
                .get(row)
                .map(|r| r.cmd)
                .unwrap_or(CommandType::None)
        })
    }

    // ---- Audio-thread entry point --------------------------------------

    /// Per-block tick: detect the transport play-start edge and emit a
    /// snapshot of all currently-enabled rows when we see it.
    ///
    /// Call this once per audio block, before draining any per-event
    /// processing.
    pub fn tick<S: EventSink>(&mut self, is_playing: bool, sink: &mut S) {
        let just_started = is_playing && !self.last_transport_playing;
        self.last_transport_playing = is_playing;

        if just_started {
            self.emit_snapshot(sink, 0);
        }
    }

    /// Emit everything the plugin should send "right now": program change
    /// (if enabled), then each enabled row's CC sequence.
    ///
    /// Exposed publicly so tests and future diff code can call it directly;
    /// the normal production path goes through [`Self::tick`].
    pub fn emit_snapshot<S: EventSink>(&mut self, sink: &mut S, timing: u32) {
        let channel = self.channel;

        if self.program_enabled {
            sink.push_program(timing, channel, self.program);
            self.last_emitted_program = Some(self.program);
        } else {
            self.last_emitted_program = None;
        }

        for row_idx in 0..MAX_ROWS {
            // Index by i to cheaply grab the row without borrowing all of
            // `self.rows` — lets us still call methods on `self` above if
            // we want (we don't yet, but the pattern is future-proof).
            let row = &self.rows[row_idx];

            if !row.enabled {
                self.last_emitted_rows[row_idx] = None;
                continue;
            }

            let cmd = CommandType::fixed_for_row(row_idx).unwrap_or(row.cmd);
            let encoded = encode_row(cmd, &row.fields);

            for msg in encoded.as_slice() {
                sink.push_cc(timing, channel, msg.cc, msg.value);
            }
            // Cache for the future diff path: next change to this row can
            // be skipped if it re-encodes to the same bytes.
            self.last_emitted_rows[row_idx] = Some(encoded);
        }

        self.last_emitted_channel = channel;
    }
}

/// Clamp to the MIDI 7-bit data range. Inlined because it's used everywhere
/// and is one instruction after inlining.
#[inline]
fn clamp_u8_127(v: u8) -> u8 {
    if v > 127 {
        127
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-side `EventSink`: records every emission as a tagged enum so we
    /// can pattern-match them in assertions.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Emission {
        Cc { timing: u32, channel: u8, cc: u8, value: u8 },
        Program { timing: u32, channel: u8, program: u8 },
    }

    #[derive(Default)]
    struct RecordingSink(Vec<Emission>);

    impl EventSink for RecordingSink {
        fn push_cc(&mut self, timing: u32, channel: u8, cc: u8, value: u8) {
            self.0.push(Emission::Cc { timing, channel, cc, value });
        }
        fn push_program(&mut self, timing: u32, channel: u8, program: u8) {
            self.0.push(Emission::Program { timing, channel, program });
        }
    }

    // Shorthands to keep tests readable.
    fn cc(timing: u32, channel: u8, cc: u8, value: u8) -> Emission {
        Emission::Cc { timing, channel, cc, value }
    }
    fn pc(timing: u32, channel: u8, program: u8) -> Emission {
        Emission::Program { timing, channel, program }
    }

    #[test]
    fn snapshot_when_transport_starts() {
        let mut core = SenderCore::new();
        core.set_channel(3);
        core.set_program(42);
        core.set_program_enabled(true);
        core.set_row_enabled(0, true); // Volume
        core.set_row_field(0, 0, 100);

        let mut sink = RecordingSink::default();
        core.tick(true, &mut sink); // play just started

        assert_eq!(
            sink.0,
            vec![
                pc(0, 3, 42),
                cc(0, 3, 0x07, 100),
            ]
        );
    }

    #[test]
    fn no_snapshot_while_stopped() {
        let mut core = SenderCore::new();
        core.set_row_enabled(0, true);
        core.set_row_field(0, 0, 100);

        let mut sink = RecordingSink::default();
        core.tick(false, &mut sink);

        assert!(sink.0.is_empty());
    }

    #[test]
    fn no_redundant_snapshot_on_continued_playback() {
        let mut core = SenderCore::new();
        core.set_row_enabled(0, true);
        core.set_row_field(0, 0, 100);

        let mut sink = RecordingSink::default();
        core.tick(true, &mut sink); // edge: emit
        let emitted_once = sink.0.len();
        core.tick(true, &mut sink); // still playing: should NOT re-emit
        assert_eq!(sink.0.len(), emitted_once);
    }

    #[test]
    fn snapshot_re_emits_after_stop_then_start() {
        let mut core = SenderCore::new();
        core.set_row_enabled(0, true);
        core.set_row_field(0, 0, 64);

        let mut sink = RecordingSink::default();
        core.tick(true, &mut sink); // emit 1
        core.tick(false, &mut sink); // stop
        core.tick(true, &mut sink); // edge: emit again

        // Each tick(true) emitted one CC (row 0, program disabled).
        assert_eq!(sink.0.len(), 2);
    }

    #[test]
    fn program_change_omitted_when_disabled() {
        let mut core = SenderCore::new();
        core.set_program(99);
        core.set_program_enabled(false);

        let mut sink = RecordingSink::default();
        core.tick(true, &mut sink);

        assert!(sink.0.iter().all(|e| !matches!(e, Emission::Program { .. })));
    }

    #[test]
    fn set_row_cmd_is_noop_for_fixed_rows() {
        let mut core = SenderCore::new();
        core.set_row_cmd(0, CommandType::MemAcc0C);
        assert_eq!(
            core.resolved_cmd_for_row(0),
            CommandType::Volume,
            "row 0 must stay as Volume regardless of user input"
        );
    }

    #[test]
    fn set_row_cmd_works_for_dynamic_rows() {
        let mut core = SenderCore::new();
        core.set_row_cmd(7, CommandType::BendRange);
        assert_eq!(core.resolved_cmd_for_row(7), CommandType::BendRange);
    }

    #[test]
    fn dynamic_row_emits_its_chosen_command() {
        let mut core = SenderCore::new();
        core.set_row_enabled(5, true);
        core.set_row_cmd(5, CommandType::MemAcc0C);
        core.set_row_field(5, 0, 1);
        core.set_row_field(5, 1, 2);
        core.set_row_field(5, 2, 3);
        core.set_row_field(5, 3, 4);

        let mut sink = RecordingSink::default();
        core.tick(true, &mut sink);

        assert_eq!(
            sink.0,
            vec![
                cc(0, 0, 0x0D, 1),
                cc(0, 0, 0x0E, 2),
                cc(0, 0, 0x0F, 3),
                cc(0, 0, 0x0C, 4),
            ]
        );
    }

    #[test]
    fn disabled_rows_are_silent() {
        let mut core = SenderCore::new();
        // Enable volume (row 0), leave all others disabled.
        core.set_row_enabled(0, true);
        core.set_row_field(0, 0, 50);

        let mut sink = RecordingSink::default();
        core.tick(true, &mut sink);

        // Exactly one CC (CC#07 from Volume), nothing from rows 1..16.
        assert_eq!(sink.0, vec![cc(0, 0, 0x07, 50)]);
    }

    #[test]
    fn clamps_out_of_range_inputs() {
        let mut core = SenderCore::new();
        core.set_channel(200); // → clamped to 15
        core.set_program(200); // → clamped to 127
        core.set_row_field(0, 0, 200); // → clamped to 127
        assert_eq!(core.channel(), 15);
        assert_eq!(core.program(), 127);
        assert_eq!(core.row(0).unwrap().fields[0], 127);
    }

    #[test]
    fn reset_wipes_everything() {
        let mut core = SenderCore::new();
        core.set_channel(5);
        core.set_program(99);
        core.set_program_enabled(true);
        core.set_row_enabled(0, true);
        core.reset();
        assert_eq!(core.channel(), 0);
        assert_eq!(core.program(), 0);
        assert!(!core.program_enabled());
        assert!(!core.row(0).unwrap().enabled);
    }

    #[test]
    fn reset_runtime_keeps_config() {
        let mut core = SenderCore::new();
        core.set_row_enabled(0, true);
        core.set_row_field(0, 0, 7);

        let mut sink = RecordingSink::default();
        core.tick(true, &mut sink); // edge: emit

        core.reset_runtime(); // simulate plugin deactivation/reactivation

        let mut sink2 = RecordingSink::default();
        core.tick(true, &mut sink2);
        // After reset_runtime, tick(true) is once again a rising edge.
        assert_eq!(sink2.0, vec![cc(0, 0, 0x07, 7)]);
        // And the row config survived.
        assert!(core.row(0).unwrap().enabled);
    }
}
