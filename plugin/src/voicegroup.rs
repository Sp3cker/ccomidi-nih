//! Voicegroup bridge — parses `poryaaaa_state.json` into typed Rust structs
//! and tracks the file's mtime so the UI can reload on demand.
//!
//! Ported from `src/plugin/voicegroup_bridge.{h,cpp}` in the C++ project.
//!
//! # File format (abridged)
//!
//! The poryaaaa synth writes a JSON sidecar describing the current
//! voicegroup and the list of instruments that can be appended to it:
//!
//! ```json
//! {
//!   "projectRoot": "/path/to/project",   // ignored
//!   "voicegroup":  "MyVoicegroup",       // used only in error messages
//!   "slots": [
//!     { "program": 0, "name": "Organ" },
//!     { "program": 1, "name": "Piano" }
//!   ],
//!   "availableInstruments": [
//!     { "name": "Strings" },
//!     { "name": "Brass" }
//!   ]
//! }
//! ```
//!
//! # Threading
//!
//! This module is pure data + I/O — no shared state. The plugin wraps
//! `VoicegroupState` in an `Arc<Mutex<…>>` to share between UI and audio.
//!
//! # Rust patterns in this file
//!
//! - `#[derive(Deserialize)]` on private `Raw*` structs lets `serde_json`
//!   walk the JSON into typed values. We then *validate* into public
//!   types (clamping `program` to u8, dropping blank entries) — untrusted
//!   input never reaches the hot path.
//! - `Option<T>` everywhere `C++` would use sentinel values like `-1`.
//! - `thiserror`-free: we stringify errors into `VoicegroupState::error`
//!   because the UI just shows them verbatim.

use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// -----------------------------------------------------------------------------
// Public types
// -----------------------------------------------------------------------------

/// One voicegroup slot — a MIDI program (0..=127) plus a display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSlot {
    pub program: u8,
    pub name: String,
}

/// Fully-loaded (or failed-to-load) voicegroup state.
///
/// On failure, `error` is populated and the vectors are empty. `state_path`
/// always reflects the file we tried to read so the UI can tell the user
/// *where* it looked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoicegroupState {
    pub slots: Vec<VoiceSlot>,
    pub available_instruments: Vec<String>,
    pub state_path: Option<PathBuf>,
    pub error: Option<String>,
    /// Last-known mtime of the state file, as nanoseconds since the UNIX
    /// epoch. `None` means "couldn't stat the file" (usually because it
    /// doesn't exist yet).
    pub mtime_ns: Option<u128>,
}

// -----------------------------------------------------------------------------
// Path discovery
// -----------------------------------------------------------------------------

/// Resolve where `poryaaaa_state.json` lives.
///
/// Precedence:
///   1. `CCOMIDI_STATE_PATH` env var — exact file path (useful for tests
///      and for advanced users who keep the state file elsewhere)
///   2. Alongside the loaded plugin binary, matching the C++ behavior:
///      - macOS: walk up `.../bundle.clap/Contents/MacOS/<binary>` four
///        levels to the bundle's parent directory
///      - Linux/Windows: the `.clap` is a single file; use its parent dir
///
/// Returns `None` only when even step 2 can't figure out our own path,
/// which shouldn't happen in practice — the shared lib was loaded by
/// name, so `dladdr(3)` knows where it is.
pub fn resolve_state_path() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("CCOMIDI_STATE_PATH") {
        return Some(PathBuf::from(override_path));
    }
    derive_state_path_from_library()
}

#[cfg(unix)]
fn derive_state_path_from_library() -> Option<PathBuf> {
    let lib = current_library_path()?;
    #[cfg(target_os = "macos")]
    {
        // lib = …/<bundle>.clap/Contents/MacOS/<binary>
        // Walk up binary → MacOS → Contents → bundle → parent.
        let parent_of_bundle = lib.parent()?.parent()?.parent()?.parent()?;
        Some(parent_of_bundle.join("poryaaaa_state.json"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        // On Linux, the .clap is a single .so — parent is the user's
        // `~/.clap` (or wherever they installed it).
        Some(lib.parent()?.join("poryaaaa_state.json"))
    }
}

#[cfg(not(unix))]
fn derive_state_path_from_library() -> Option<PathBuf> {
    // Windows fallback: no dladdr. Use CWD as a last-ditch attempt; users
    // on Windows should set CCOMIDI_STATE_PATH explicitly.
    Some(std::env::current_dir().ok()?.join("poryaaaa_state.json"))
}

#[cfg(unix)]
fn current_library_path() -> Option<PathBuf> {
    use std::ffi::CStr;
    // SAFETY: `dladdr` only writes to the Dl_info we hand it. `addr`
    // points at a function in this very dylib, which is what we want
    // dladdr to look up.
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        let addr = current_library_path as *const libc::c_void;
        if libc::dladdr(addr, &mut info) == 0 {
            return None;
        }
        if info.dli_fname.is_null() {
            return None;
        }
        let cstr = CStr::from_ptr(info.dli_fname);
        Some(PathBuf::from(cstr.to_str().ok()?))
    }
}

// -----------------------------------------------------------------------------
// Loading & parsing
// -----------------------------------------------------------------------------

/// Private raw shape used only for serde. We validate into public types
/// below so untrusted fields (like a negative `program`) can't leak in.
#[derive(Deserialize)]
struct RawState {
    #[serde(default)]
    slots: Vec<RawSlot>,
    #[serde(default, rename = "availableInstruments")]
    available_instruments: Vec<RawInstrument>,
}

#[derive(Deserialize)]
struct RawSlot {
    #[serde(default)]
    program: i64,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct RawInstrument {
    #[serde(default)]
    name: String,
}

/// Read + parse the state file. Always returns a `VoicegroupState` —
/// failures are recorded in `error` rather than bubbled up, so the UI can
/// display them without any extra glue.
pub fn load_state(path: &std::path::Path) -> VoicegroupState {
    let mut out = VoicegroupState {
        state_path: Some(path.to_path_buf()),
        ..Default::default()
    };

    // Stat first. If the file doesn't exist yet, the user probably hasn't
    // loaded poryaaaa in the DAW — give them that specific hint.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            out.error = Some(
                "poryaaaa hasn't written its state yet — load poryaaaa in the DAW."
                    .to_string(),
            );
            return out;
        }
    };

    out.mtime_ns = meta.modified().ok().and_then(system_time_to_ns);

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            out.error = Some("Could not read poryaaaa_state.json.".to_string());
            return out;
        }
    };

    let raw: RawState = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            out.error = Some(format!("JSON parse error: {e}"));
            return out;
        }
    };

    // Validate slots: drop entries with out-of-range program or blank name.
    for slot in raw.slots {
        if slot.program < 0 || slot.program > 127 || slot.name.is_empty() {
            continue;
        }
        out.slots.push(VoiceSlot {
            program: slot.program as u8,
            name: slot.name,
        });
    }

    // Available instruments: just collect the non-empty names.
    for inst in raw.available_instruments {
        if inst.name.is_empty() {
            continue;
        }
        out.available_instruments.push(inst.name);
    }

    if out.slots.is_empty() {
        out.error = Some("state.json has no sample-bearing slots.".to_string());
    }

    out
}

/// Return the file's current mtime without reading it. Cheap; fine to
/// call on every UI tick if needed.
pub fn current_mtime_ns(path: &std::path::Path) -> Option<u128> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .and_then(system_time_to_ns)
}

fn system_time_to_ns(t: SystemTime) -> Option<u128> {
    t.duration_since(UNIX_EPOCH).ok().map(duration_to_ns)
}

fn duration_to_ns(d: Duration) -> u128 {
    d.as_secs() as u128 * 1_000_000_000 + d.subsec_nanos() as u128
}

// -----------------------------------------------------------------------------
// 14-bit CC range (CC#98 LSB + CC#99 MSB)
// -----------------------------------------------------------------------------

/// Max valid index for the Add-Instrument CC pair.
///
/// The index is transmitted as two 7-bit CCs (LSB = CC#98, MSB = CC#99),
/// giving a 14-bit value: 0..=16383.
pub const MAX_INSTRUMENT_INDEX: u32 = 0x3FFF; // 16383

/// Sentinel meaning "nothing queued" for the audio-thread-observable
/// pending-add-instrument atomic.
pub const NO_PENDING_INSTRUMENT: i32 = -1;

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, json: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("ccomidi-vg-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn parses_minimal_valid_state() {
        let path = write_tmp(
            "minimal.json",
            r#"{"slots":[{"program":0,"name":"Organ"}]}"#,
        );
        let state = load_state(&path);
        assert!(state.error.is_none(), "error: {:?}", state.error);
        assert_eq!(
            state.slots,
            vec![VoiceSlot {
                program: 0,
                name: "Organ".into()
            }]
        );
        assert!(state.available_instruments.is_empty());
        assert!(state.mtime_ns.is_some());
    }

    #[test]
    fn parses_full_state_with_instruments() {
        let path = write_tmp(
            "full.json",
            r#"{
                "projectRoot": "/tmp",
                "voicegroup": "VG",
                "slots": [
                    {"program": 10, "name": "Piano"},
                    {"program": 20, "name": "Bass"}
                ],
                "availableInstruments": [
                    {"name": "Strings"},
                    {"name": "Brass"}
                ]
            }"#,
        );
        let state = load_state(&path);
        assert!(state.error.is_none());
        assert_eq!(state.slots.len(), 2);
        assert_eq!(state.slots[0].program, 10);
        assert_eq!(state.slots[1].name, "Bass");
        assert_eq!(state.available_instruments, vec!["Strings", "Brass"]);
    }

    #[test]
    fn drops_out_of_range_slots_silently() {
        let path = write_tmp(
            "range.json",
            r#"{"slots":[
                {"program": -1, "name": "Bogus"},
                {"program": 999, "name": "AlsoBogus"},
                {"program": 5, "name": ""},
                {"program": 7, "name": "Ok"}
            ]}"#,
        );
        let state = load_state(&path);
        assert_eq!(state.slots.len(), 1);
        assert_eq!(state.slots[0].program, 7);
        assert_eq!(state.slots[0].name, "Ok");
    }

    #[test]
    fn missing_file_reports_friendly_error() {
        let path = std::env::temp_dir().join("ccomidi-vg-tests/does-not-exist.json");
        let _ = std::fs::remove_file(&path);
        let state = load_state(&path);
        assert!(state.error.as_deref().unwrap_or("").contains("poryaaaa"));
        assert!(state.slots.is_empty());
        assert_eq!(state.mtime_ns, None);
    }

    #[test]
    fn malformed_json_reports_parse_error() {
        let path = write_tmp("bad.json", "{this is not json");
        let state = load_state(&path);
        assert!(state.error.as_deref().unwrap_or("").contains("JSON"));
    }

    #[test]
    fn empty_slots_reports_error_but_loads_instruments() {
        let path = write_tmp(
            "empty.json",
            r#"{"slots":[], "availableInstruments":[{"name":"X"}]}"#,
        );
        let state = load_state(&path);
        assert!(state.error.is_some()); // "no sample-bearing slots"
        assert_eq!(state.available_instruments, vec!["X"]);
    }

    #[test]
    fn env_var_overrides_path_resolution() {
        let tmp = std::env::temp_dir().join("ccomidi-test-override.json");
        std::env::set_var("CCOMIDI_STATE_PATH", tmp.to_str().unwrap());
        let resolved = resolve_state_path().unwrap();
        assert_eq!(resolved, tmp);
        std::env::remove_var("CCOMIDI_STATE_PATH");
    }
}
