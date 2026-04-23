//! Voicegroup bridge — parses `poryaaaa_state.json` into typed Rust structs
//! and tracks the file's mtime so the UI can reload on demand.
//!
//! Ported from `src/plugin/voicegroup_bridge.{h,cpp}` in the C++ project.
//!
//! # File format (abridged)
//!
//! The poryaaaa synth writes a JSON sidecar describing the current
//! voicegroup:
//!
//! ```json
//! {
//!   "projectRoot": "/path/to/project",   // ignored
//!   "voicegroup":  "MyVoicegroup",       // used only in error messages
//!   "slots": [
//!     { "program": 0, "name": "Organ" },
//!     { "program": 1, "name": "Piano" }
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
use std::path::{Path, PathBuf};
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
///   2. Candidate list (first existing file wins):
///      - `<N levels up from the loaded .so/bundle>/poryaaaa_state.json`
///        for a handful of N's, covering:
///          · macOS bundle layout (`bundle.vst3/Contents/MacOS/binary`)
///            → 4 levels
///          · Linux/Windows single-file `.vst3` → 1 level
///          · Bare dev install (`target/bundled/X.vst3/…`) resolves to
///            `target/bundled/` which is probably not where poryaaaa
///            wrote; higher levels may be the right answer if the user
///            symlinked the bundle into their VST3 dir
///      - The canonicalized version of the above (handles the case
///        where `dladdr` returned the symlink target instead of the
///        install path)
///      - `~/Library/Audio/Plug-Ins/VST3/poryaaaa_state.json` on macOS
///        / `~/.vst3/poryaaaa_state.json` on Linux — the standard
///        user-scope VST3 plugin directory where poryaaaa itself likely
///        lives
///   3. If no candidate exists on disk, returns the first one anyway so
///      the UI error message can point the user at *where* we looked.
pub fn resolve_state_path() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("CCOMIDI_STATE_PATH") {
        return Some(PathBuf::from(override_path));
    }

    let candidates = candidate_state_paths();
    if let Some(found) = candidates.iter().find(|p| p.exists()) {
        return Some(found.clone());
    }
    candidates.into_iter().next()
}

/// Every place we're willing to look for `poryaaaa_state.json`, in
/// priority order. Exposed to callers only through `resolve_state_path`.
fn candidate_state_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    // Walk up the dladdr-reported dylib path, AND its canonical form.
    // Canonicalization follows symlinks — useful when the host loaded
    // us via a symlink but dladdr returned the target path (or vice
    // versa).
    #[cfg(unix)]
    {
        if let Some(lib) = current_library_path() {
            out.extend(state_candidates_around(&lib));
            if let Ok(canonical) = std::fs::canonicalize(&lib) {
                if canonical != lib {
                    out.extend(state_candidates_around(&canonical));
                }
            }
        }
    }

    // User-scope VST3 directory fallback — this is the install location
    // for poryaaaa, so `poryaaaa_state.json` probably ends up here even
    // if ccomidi-nih is living somewhere else (dev build, alt install).
    #[cfg(target_os = "macos")]
    if let Ok(home) = std::env::var("HOME") {
        out.push(
            PathBuf::from(home).join("Library/Audio/Plug-Ins/VST3/poryaaaa_state.json"),
        );
    }
    #[cfg(target_os = "linux")]
    if let Ok(home) = std::env::var("HOME") {
        out.push(PathBuf::from(home).join(".vst3/poryaaaa_state.json"));
    }

    // Windows fallback — dladdr isn't available, so we rely on env-var
    // or CWD. Users on Windows should set `CCOMIDI_STATE_PATH`.
    #[cfg(not(unix))]
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("poryaaaa_state.json"));
    }

    // Deduplicate while preserving order.
    out.dedup();
    out
}

/// Build the candidate list "walk up from `lib` looking for siblings".
///
/// We walk up to 5 levels. That covers:
///   - Linux/Windows flat `.vst3`: level 1
///   - macOS bundle: level 4 (bundle/Contents/MacOS/binary)
///   - anything deeper (dev builds nested inside target/bundled/): the
///     higher levels are still plausible locations if the user symlinked
///     the bundle into the OS VST3 dir
fn state_candidates_around(lib: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let mut dir = lib.parent();
    for _ in 0..5 {
        match dir {
            Some(d) => {
                v.push(d.join("poryaaaa_state.json"));
                dir = d.parent();
            }
            None => break,
        }
    }
    v
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
}

#[derive(Deserialize)]
struct RawSlot {
    #[serde(default)]
    program: i64,
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
    // loaded poryaaaa in the DAW — give them that specific hint, and
    // include the path we looked at so the user can verify or set
    // `CCOMIDI_STATE_PATH` to point elsewhere.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            out.error = Some(format!(
                "poryaaaa_state.json not found at {}. Load poryaaaa in the DAW, \
                 or set $CCOMIDI_STATE_PATH to override.",
                path.display()
            ));
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
// Instrument-kind classification (name heuristic)
// -----------------------------------------------------------------------------

/// What kind of synthesis backs a voicegroup slot, inferred from the slot's
/// display name. Used by the editor to pick the right Pan UI: the GBA
/// hardware channels (Square 1/2, Noise, Programmable Wave) only expose
/// Pan as hard-L / Center / hard-R, while DirectSound samples (and any
/// container that resolves to DS — keysplits, drumsets) support the full
/// continuous 0..=127 range.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InstrumentKind {
    /// Square-wave hardware channel. Pan snaps to L/C/R.
    Square,
    /// Noise hardware channel. Pan snaps to L/C/R.
    Noise,
    /// Programmable-wave hardware channel. Pan snaps to L/C/R.
    ProgWave,
    /// DirectSound sample, keysplit, drumset, or anything else we don't
    /// recognize. Pan is continuous 0..=127.
    Other,
}

impl InstrumentKind {
    /// True if Pan on this instrument should be presented as a 3-way
    /// enumeration (Left / Center / Right) rather than a 0..=127 slider.
    pub fn is_enum_pan(&self) -> bool {
        matches!(self, Self::Square | Self::Noise | Self::ProgWave)
    }
}

/// Heuristically classify a slot name into an [`InstrumentKind`].
///
/// Safe-by-default: anything that doesn't clearly look like a hardware
/// channel falls through to [`InstrumentKind::Other`], which keeps the
/// continuous Pan slider. That's the right bias — showing a slider on a
/// hardware channel is a minor UX miss, but snapping a DirectSound
/// sample's Pan to only three values would quietly destroy the user's
/// intent.
///
/// The patterns match the `poryaaaa_state.json` slot-name convention
/// observed in practice (e.g. "Square 1", "Noise", "ProgWave 1"). Names
/// are trimmed and lowercased before matching so "SQUARE 1", " square 1",
/// and "Square 1" all classify the same way.
pub fn classify_name(name: &str) -> InstrumentKind {
    let n = name.trim().to_lowercase();

    // "square", "square 1", "square_2", "squarewave", "square alt", …
    if n.starts_with("square") || n.starts_with("sq ") {
        return InstrumentKind::Square;
    }

    // Bare "noise" / "noise 1" / "noise alt". We intentionally match the
    // *start* of the name, so available-instrument names like
    // "register_noise" (a DirectSound sample with "noise" in the middle)
    // stay classified as Other.
    if n.starts_with("noise") {
        return InstrumentKind::Noise;
    }

    // "progwave", "prog wave", "prog_wave", "programmable wave", "pwave".
    if n.starts_with("progwave")
        || n.starts_with("prog wave")
        || n.starts_with("prog_wave")
        || n.starts_with("programmable wave")
        || n.starts_with("pwave")
    {
        return InstrumentKind::ProgWave;
    }

    InstrumentKind::Other
}

/// Look up the currently-loaded instrument for a program number and
/// classify it. Returns `Other` if the program isn't present in the
/// voicegroup (unconfigured slot → show the slider rather than
/// guessing).
pub fn kind_for_program(slots: &[VoiceSlot], program: u8) -> InstrumentKind {
    slots
        .iter()
        .find(|s| s.program == program)
        .map(|s| classify_name(&s.name))
        .unwrap_or(InstrumentKind::Other)
}

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
        assert!(state.mtime_ns.is_some());
    }

    #[test]
    fn parses_full_state() {
        let path = write_tmp(
            "full.json",
            r#"{
                "projectRoot": "/tmp",
                "voicegroup": "VG",
                "slots": [
                    {"program": 10, "name": "Piano"},
                    {"program": 20, "name": "Bass"}
                ]
            }"#,
        );
        let state = load_state(&path);
        assert!(state.error.is_none());
        assert_eq!(state.slots.len(), 2);
        assert_eq!(state.slots[0].program, 10);
        assert_eq!(state.slots[1].name, "Bass");
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
    fn empty_slots_reports_error() {
        let path = write_tmp("empty.json", r#"{"slots":[]}"#);
        let state = load_state(&path);
        assert!(state.error.is_some()); // "no sample-bearing slots"
    }

    #[test]
    fn env_var_overrides_path_resolution() {
        let tmp = std::env::temp_dir().join("ccomidi-test-override.json");
        std::env::set_var("CCOMIDI_STATE_PATH", tmp.to_str().unwrap());
        let resolved = resolve_state_path().unwrap();
        assert_eq!(resolved, tmp);
        std::env::remove_var("CCOMIDI_STATE_PATH");
    }

    #[test]
    fn classifies_hardware_channel_names() {
        assert_eq!(classify_name("Square 1"), InstrumentKind::Square);
        assert_eq!(classify_name("square 2"), InstrumentKind::Square);
        assert_eq!(classify_name("Square_alt"), InstrumentKind::Square);
        assert_eq!(classify_name("SquareWave"), InstrumentKind::Square);
        assert_eq!(classify_name("Noise"), InstrumentKind::Noise);
        assert_eq!(classify_name("noise alt"), InstrumentKind::Noise);
        assert_eq!(classify_name("ProgWave 1"), InstrumentKind::ProgWave);
        assert_eq!(classify_name("prog_wave"), InstrumentKind::ProgWave);
        assert_eq!(classify_name("Programmable Wave"), InstrumentKind::ProgWave);
    }

    #[test]
    fn classifies_directsound_and_containers_as_other() {
        // These names were pulled from a real poryaaaa_state.json —
        // anything DirectSound-backed, or a container (keysplit /
        // drumset / nested voicegroup), must stay on the continuous
        // pan slider.
        assert_eq!(classify_name("16.pcm"), InstrumentKind::Other);
        assert_eq!(
            classify_name("sc88pro_pizzicato_strings.bin"),
            InstrumentKind::Other
        );
        assert_eq!(
            classify_name("voicegroup_piano_keysplit"),
            InstrumentKind::Other
        );
        assert_eq!(classify_name("voicegroup192"), InstrumentKind::Other);
        // "register_noise" has "noise" as a *suffix*; it's a DS sample,
        // not the Noise hardware channel. Our `starts_with` heuristic
        // keeps it classified as Other.
        assert_eq!(classify_name("register_noise"), InstrumentKind::Other);
    }

    #[test]
    fn kind_for_program_uses_matching_slot() {
        let slots = vec![
            VoiceSlot {
                program: 0,
                name: "Square 1".into(),
            },
            VoiceSlot {
                program: 2,
                name: "16.pcm".into(),
            },
            VoiceSlot {
                program: 7,
                name: "Noise".into(),
            },
        ];
        assert_eq!(kind_for_program(&slots, 0), InstrumentKind::Square);
        assert_eq!(kind_for_program(&slots, 2), InstrumentKind::Other);
        assert_eq!(kind_for_program(&slots, 7), InstrumentKind::Noise);
        // Unknown program → Other (safe default: show the slider).
        assert_eq!(kind_for_program(&slots, 5), InstrumentKind::Other);
    }
}
