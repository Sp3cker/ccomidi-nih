//! `CommandType` enum and row-layout constants.
//!
//! Mirrors the C++ definitions in `src/core/sender_core.h`. Pure values only —
//! no mutable state — so this module is trivially testable and can't drift
//! from the original by accidentally gaining behavior.

/// Total number of configurable rows in the UI and in serialized state.
/// Matches `kMaxCommandRows` in the C++ code.
pub const MAX_ROWS: usize = 16;

/// The first `FIXED_ROW_COUNT` rows have *fixed* [`CommandType`]s
/// (Volume / Pan / Mod / LfoSpeed) — the user cannot change what CC they
/// emit, only the value. Rows `[FIXED_ROW_COUNT..MAX_ROWS)` are freely
/// assignable.
pub const FIXED_ROW_COUNT: usize = 4;

/// Every row carries a fixed-size bank of 4 field values. Not every
/// `CommandType` uses all 4 fields; unused fields are ignored at encode time.
pub const MAX_FIELDS: usize = 4;

/// Maximum number of MIDI CC messages a single row may emit. `MemAcc*`
/// encodings use 4 messages; everything else uses 1 or 2. We keep this
/// small so [`crate::core::EncodedCommand`] stays trivially stack-allocatable.
pub const MAX_MESSAGES_PER_ROW: usize = 5;

/// What a row encodes when enabled.
///
/// # Rust idioms worth noting
///
/// `#[repr(u8)]` pins each variant's discriminant to a specific single byte.
/// That's not strictly required, but it:
///   1. keeps [`RowState`](super::RowState) compact in memory,
///   2. makes serialization / deserialization as an integer trivial, and
///   3. lines up with the original C++ enum's byte values, so any tool that
///      already reads ccomidi state files continues to work.
///
/// `#[derive(Copy, Clone, …)]` auto-generates the listed traits. `Copy` is
/// safe here because the type is just a tagged byte; `let a = b; let c = b;`
/// works without explicit `.clone()`.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum CommandType {
    // ---- Fixed-row commands (rows 0..4) ----------------------------------
    /// Fixed row 0. Emits CC#0x07 (Channel Volume).
    Volume = 0,
    /// Fixed row 1. Emits CC#0x0A (Pan).
    Pan = 1,
    /// Fixed row 2. Emits CC#0x01 (Modulation Wheel).
    Mod = 2,
    /// Fixed row 3. Emits CC#0x15 (synth-specific LFO speed).
    LfoSpeed = 3,

    // ---- Assignable commands (dynamic rows) -----------------------------
    /// Dynamic row disabled → emits nothing. Default for dynamic rows.
    None = 4,

    /// CC#0x14 (pitch-bend range).
    BendRange = 5,
    /// CC#0x16 (modulation target / type).
    ModType = 6,
    /// CC#0x18 (fine tune).
    Tune = 7,
    /// CC#0x1A (LFO delay).
    LfoDelay = 8,
    /// CC#0x21 (synth-specific voice priority).
    Priority21 = 9,
    /// CC#0x27 (alternate priority target).
    Priority27 = 10,

    /// Two-CC "extended command": CC#0x1E = 0x08, then CC#0x1D = field[0].
    ///
    /// Mirrors the `xcmd iecv` instruction in the original synth.
    XcmdIecv = 11,
    /// Two-CC "extended command": CC#0x1E = 0x09, then CC#0x1D = field[0].
    XcmdIecl = 12,

    /// Four-CC memory-access write into "bank 0C":
    /// CC#0x0D=f0, CC#0x0E=f1, CC#0x0F=f2, CC#0x0C=f3.
    ///
    /// The *last* CC doubles as the bank selector (it's the committing write).
    MemAcc0C = 13,
    /// Same as [`Self::MemAcc0C`] but committing through "bank 10":
    /// CC#0x0D=f0, CC#0x0E=f1, CC#0x0F=f2, CC#0x10=f3.
    MemAcc10 = 14,
}

impl CommandType {
    /// Return the hardcoded [`CommandType`] for a fixed row (0..FIXED_ROW_COUNT),
    /// or `None` for a dynamic row.
    ///
    /// `Option<T>` is Rust's native "nullable": either `Some(value)` or `None`.
    /// The compiler forces callers to handle both cases — no null-deref bugs.
    pub const fn fixed_for_row(row: usize) -> Option<Self> {
        match row {
            0 => Some(Self::Volume),
            1 => Some(Self::Pan),
            2 => Some(Self::Mod),
            3 => Some(Self::LfoSpeed),
            _ => None,
        }
    }

    /// True iff this is one of the four fixed-row commands.
    pub const fn is_fixed(self) -> bool {
        matches!(
            self,
            Self::Volume | Self::Pan | Self::Mod | Self::LfoSpeed
        )
    }

    /// Reconstruct a [`CommandType`] from its stored discriminant byte.
    ///
    /// Returns `None` on unknown values — which can happen if state from a
    /// newer plugin version is loaded into an older one. Callers should
    /// treat `None` as "reset this row" rather than panic.
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Volume,
            1 => Self::Pan,
            2 => Self::Mod,
            3 => Self::LfoSpeed,
            4 => Self::None,
            5 => Self::BendRange,
            6 => Self::ModType,
            7 => Self::Tune,
            8 => Self::LfoDelay,
            9 => Self::Priority21,
            10 => Self::Priority27,
            11 => Self::XcmdIecv,
            12 => Self::XcmdIecl,
            13 => Self::MemAcc0C,
            14 => Self::MemAcc10,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    //! `#[cfg(test)]` means this whole module is compiled *only* under
    //! `cargo test`. It is stripped from the release plugin binary.

    use super::*;

    #[test]
    fn fixed_rows_are_pinned() {
        assert_eq!(CommandType::fixed_for_row(0), Some(CommandType::Volume));
        assert_eq!(CommandType::fixed_for_row(1), Some(CommandType::Pan));
        assert_eq!(CommandType::fixed_for_row(2), Some(CommandType::Mod));
        assert_eq!(CommandType::fixed_for_row(3), Some(CommandType::LfoSpeed));
    }

    #[test]
    fn dynamic_rows_are_not_fixed() {
        for row in FIXED_ROW_COUNT..MAX_ROWS {
            assert!(CommandType::fixed_for_row(row).is_none());
        }
    }

    #[test]
    fn roundtrip_through_u8() {
        // For each variant we care about, the discriminant → enum → discriminant
        // round-trip should be lossless.
        for v in 0u8..=14 {
            let ct = CommandType::from_u8(v).expect("valid");
            assert_eq!(ct as u8, v);
        }
    }

    #[test]
    fn unknown_discriminant_is_none() {
        assert!(CommandType::from_u8(99).is_none());
    }
}
