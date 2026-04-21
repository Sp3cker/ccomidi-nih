//! Turn a row's `(CommandType, fields)` into a short sequence of MIDI CC
//! messages.
//!
//! This is a *pure* function — no mutable state, no I/O, no dependency on any
//! framework. That makes it trivial to unit-test exhaustively and to reason
//! about.
//!
//! Mirrors `encode_row` in the C++ code (see `src/core/sender_core.cpp`
//! around line 172).

use super::command::{CommandType, MAX_FIELDS, MAX_MESSAGES_PER_ROW};

/// One raw MIDI Control Change message.
///
/// The MIDI status byte (`0xB0 | channel`) is *not* baked in here — the caller
/// attaches the channel at emission time. That matches the C++ design where
/// the target channel can change between row-construction time and the actual
/// emit, and lets us diff an `EncodedCommand` without its channel affecting
/// equality.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct CcMessage {
    pub cc: u8,
    pub value: u8,
}

/// Fixed-capacity buffer of CC messages describing what one row emits.
///
/// # Why not `Vec<CcMessage>`?
///
/// `Vec` allocates on the heap, which is forbidden in real-time audio code
/// (a blocking allocator call can cause audio dropouts). A plain array with
/// an explicit length keeps everything on the stack. Total size is
/// `MAX_MESSAGES_PER_ROW * 2 + 1` bytes ≈ 11 bytes — so `Copy` is cheap
/// and we pass it around by value without a second thought.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct EncodedCommand {
    pub messages: [CcMessage; MAX_MESSAGES_PER_ROW],
    /// How many entries in `messages` are meaningful.
    pub len: u8,
}

impl EncodedCommand {
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Borrow only the filled slots as a slice.
    ///
    /// Returning `&[CcMessage]` (a borrow) instead of `Vec<CcMessage>` (an
    /// owned heap buffer) means zero allocation and zero copy — the caller
    /// just iterates in place.
    pub fn as_slice(&self) -> &[CcMessage] {
        &self.messages[..self.len as usize]
    }

    fn push(&mut self, cc: u8, value: u8) {
        // `debug_assert!` is compiled out of release builds, so this is free
        // in the shipping plugin but catches mistakes during `cargo test`.
        debug_assert!((self.len as usize) < MAX_MESSAGES_PER_ROW);
        self.messages[self.len as usize] = CcMessage { cc, value };
        self.len += 1;
    }
}

/// Encode one row: given its [`CommandType`] and the 4 field values, produce
/// the 0..N MIDI CC messages to emit.
///
/// Cross-reference: see `encode_row` in `src/core/sender_core.cpp` in the
/// original C++ project.
pub fn encode_row(cmd: CommandType, fields: &[u8; MAX_FIELDS]) -> EncodedCommand {
    let mut out = EncodedCommand::default();

    // Rust's `match` is exhaustive — the compiler will fail to build if we
    // add a new CommandType variant and forget to handle it here. That's
    // how we keep this logic in lockstep with the enum.
    match cmd {
        // ---- Fixed rows & single-CC assignable commands ---------------
        CommandType::Volume => out.push(0x07, fields[0]),
        CommandType::Pan => out.push(0x0A, fields[0]),
        CommandType::Mod => out.push(0x01, fields[0]),
        CommandType::LfoSpeed => out.push(0x15, fields[0]),
        CommandType::BendRange => out.push(0x14, fields[0]),
        CommandType::ModType => out.push(0x16, fields[0]),
        CommandType::Tune => out.push(0x18, fields[0]),
        CommandType::LfoDelay => out.push(0x1A, fields[0]),
        CommandType::Priority21 => out.push(0x21, fields[0]),
        CommandType::Priority27 => out.push(0x27, fields[0]),

        // ---- Two-CC extended commands: opcode select, then value ------
        CommandType::XcmdIecv => {
            out.push(0x1E, 0x08);
            out.push(0x1D, fields[0]);
        }
        CommandType::XcmdIecl => {
            out.push(0x1E, 0x09);
            out.push(0x1D, fields[0]);
        }

        // ---- Four-CC memory-access writes: last CC selects the bank ---
        CommandType::MemAcc0C => {
            out.push(0x0D, fields[0]);
            out.push(0x0E, fields[1]);
            out.push(0x0F, fields[2]);
            out.push(0x0C, fields[3]); // bank commit
        }
        CommandType::MemAcc10 => {
            out.push(0x0D, fields[0]);
            out.push(0x0E, fields[1]);
            out.push(0x0F, fields[2]);
            out.push(0x10, fields[3]); // bank commit
        }

        // ---- Disabled dynamic row -------------------------------------
        CommandType::None => { /* emit nothing */ }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny helper so test bodies aren't cluttered with `[a, 0, 0, 0]` noise.
    fn f(a: u8, b: u8, c: u8, d: u8) -> [u8; 4] {
        [a, b, c, d]
    }

    #[test]
    fn volume_emits_cc07() {
        let e = encode_row(CommandType::Volume, &f(100, 0, 0, 0));
        assert_eq!(
            e.as_slice(),
            &[CcMessage { cc: 0x07, value: 100 }]
        );
    }

    #[test]
    fn pan_emits_cc0a() {
        let e = encode_row(CommandType::Pan, &f(64, 0, 0, 0));
        assert_eq!(
            e.as_slice(),
            &[CcMessage { cc: 0x0A, value: 64 }]
        );
    }

    #[test]
    fn mod_emits_cc01() {
        let e = encode_row(CommandType::Mod, &f(77, 0, 0, 0));
        assert_eq!(e.as_slice(), &[CcMessage { cc: 0x01, value: 77 }]);
    }

    #[test]
    fn lfo_speed_emits_cc15() {
        let e = encode_row(CommandType::LfoSpeed, &f(12, 0, 0, 0));
        assert_eq!(e.as_slice(), &[CcMessage { cc: 0x15, value: 12 }]);
    }

    #[test]
    fn xcmd_iecv_opcode_then_value() {
        let e = encode_row(CommandType::XcmdIecv, &f(42, 0, 0, 0));
        assert_eq!(
            e.as_slice(),
            &[
                CcMessage { cc: 0x1E, value: 0x08 },
                CcMessage { cc: 0x1D, value: 42 },
            ]
        );
    }

    #[test]
    fn xcmd_iecl_differs_from_iecv_only_in_opcode() {
        let iecv = encode_row(CommandType::XcmdIecv, &f(7, 0, 0, 0));
        let iecl = encode_row(CommandType::XcmdIecl, &f(7, 0, 0, 0));
        assert_ne!(iecv.messages[0], iecl.messages[0]);
        assert_eq!(iecv.messages[1], iecl.messages[1]);
    }

    #[test]
    fn mem_acc_0c_sequence_and_bank_commit() {
        let e = encode_row(CommandType::MemAcc0C, &f(1, 2, 3, 4));
        assert_eq!(
            e.as_slice(),
            &[
                CcMessage { cc: 0x0D, value: 1 },
                CcMessage { cc: 0x0E, value: 2 },
                CcMessage { cc: 0x0F, value: 3 },
                CcMessage { cc: 0x0C, value: 4 }, // 0C-bank commit
            ]
        );
    }

    #[test]
    fn mem_acc_10_only_differs_in_bank_commit() {
        let a = encode_row(CommandType::MemAcc0C, &f(1, 2, 3, 4));
        let b = encode_row(CommandType::MemAcc10, &f(1, 2, 3, 4));
        assert_eq!(a.messages[0..3], b.messages[0..3]);
        assert_ne!(a.messages[3], b.messages[3]);
        assert_eq!(b.messages[3], CcMessage { cc: 0x10, value: 4 });
    }

    #[test]
    fn none_emits_nothing() {
        let e = encode_row(CommandType::None, &f(99, 99, 99, 99));
        assert!(e.is_empty());
        assert_eq!(e.as_slice(), &[]);
    }
}
