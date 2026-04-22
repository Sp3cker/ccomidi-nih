//! Vizia editor — the plugin window the host shows when the user opens
//! ccomidi's UI.
//!
//! # Layout (top → bottom)
//!
//!   1. Title row
//!   2. Transport row: MIDI channel, Program-enable toggle, Program number
//!   3. "Fixed commands" section: Volume / Pan / Mod / LFO Speed (4 rows)
//!   4. "Voicegroup" section: status + Reload, instrument dropdown + Add
//!   5. (hidden for perf diag) "Dynamic commands": 12 freely-assignable rows
//!
//! # Vizia idioms used here
//!
//! - `#[derive(Lens)]` on `Data` generates type-safe projectors so the UI
//!   can observe `params` reactively: `Data::params` is a *lens*, not a
//!   field access.
//! - `ParamSlider` / `ParamButton` take the root lens plus a
//!   `Fn(&Params) -> &Param` closure that picks out one leaf. The closure
//!   must be `'static + Copy` so we capture `i: usize` by `move` inside
//!   each loop.
//! - Non-param UI state (voicegroup status, instrument list, selection)
//!   lives in `Data` as plain fields. It's mutated inside `Model::event`
//!   in response to `AppEvent`s emitted from button presses.

use nih_plug::params::BoolParam;
use nih_plug::prelude::{Editor, Param};
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, RwLock};

use crate::params::{CComidiParams, DYN_ROWS, FIXED_ROWS};
use crate::voicegroup::{self, VoiceSlot, VoicegroupState};

/// User-facing labels for the six fixed rows. Indexed 0..FIXED_ROWS.
/// The enabled toggle is now a plain colored square (see `bool_toggle`);
/// these labels sit beside it so the user can read what the row does.
const FIXED_ROW_LABELS: [&str; FIXED_ROWS] = [
    "Volume", "Pan", "Mod", "LFO Speed", "xCIEV", "xCIEL",
];

/// Shared interaction stylesheet.
///
/// Adding `.class("tap")` to any interactive widget opts it into a
/// brief scale-down on press — the tactile feedback principle from the
/// make-interfaces-feel-better skill. Vizia's CSS supports the `scale`
/// property directly (not via `transform`); transitions are declared
/// with `transition: <property> <duration>`.
///
/// 0.96 is the specific value the skill calls for (never below 0.95).
const INTERACTION_CSS: &str = "
.tap {
    scale: 1;
    transition: scale 100ms;
}
.tap:active {
    scale: 0.96;
}
";

/// Which page of the editor is currently visible.
///
/// Vizia's `Data` trait is required for any type used as a lens target.
/// We implement it manually here — `#[derive(Data)]` can't resolve the
/// trait name because our own `struct Data` (the root model) shadows
/// Vizia's `Data` in this file's scope.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Tab {
    /// Output channel, Program, Fixed commands, Voicegroup.
    Main,
    /// The 10 freely-assignable (non-fixed) rows.
    Additional,
}

impl nih_plug_vizia::vizia::prelude::Data for Tab {
    fn same(&self, other: &Self) -> bool {
        self == other
    }
}

// `VoiceSlot` needs `Data` so `Vec<VoiceSlot>` can be a lens target on
// `Data::slots`. Can't derive it in `voicegroup.rs` without dragging the
// vizia prelude into that otherwise framework-agnostic module, so the
// impl lives here (the orphan rule lets us do this because `VoiceSlot`
// is our own type).
impl nih_plug_vizia::vizia::prelude::Data for VoiceSlot {
    fn same(&self, other: &Self) -> bool {
        self == other
    }
}

/// Messages the editor fires at itself.
///
/// - `ReloadVoicegroup` rereads `poryaaaa_state.json`, refreshes
///   `Data::vg_status`, and updates the instrument list behind the
///   dropdown.
/// - `SelectInstrument(idx)` is fired by the PickList when the user
///   picks an entry. We immediately store the index into the shared
///   `pending_add_instrument` atomic so the audio thread emits CC#98/#99
///   on its next block — matching the C++ "pick = send" UX. The
///   selection itself is transient (not persisted).
/// - `SwitchTab(tab)` flips the top-level page.
#[derive(Debug)]
enum AppEvent {
    ReloadVoicegroup,
    SelectInstrument(usize),
    SwitchTab(Tab),
    /// User edited the track-label textbox. Writes through to the
    /// persistent `Arc<RwLock<String>>` on the params struct so the value
    /// survives project reloads.
    SetTrackLabel(String),
}

/// Root Vizia model.
#[derive(Lens)]
struct Data {
    params: Arc<CComidiParams>,
    pending_add_instrument: Arc<AtomicI32>,
    /// One-line summary for the UI. `String` (rather than the full
    /// `VoicegroupState`) because Vizia's `Data` trait is already
    /// implemented for `String` — no manual impl needed.
    vg_status: String,
    /// Current voicegroup slots (program → instrument mapping). The Pan
    /// row uses this to decide whether the loaded instrument is a
    /// hardware channel (L/C/R enum pan) or DirectSound (continuous
    /// pan). Refreshed whenever [`AppEvent::ReloadVoicegroup`] fires.
    slots: Vec<VoiceSlot>,
    /// Ordered list of instrument display names — the backing data for
    /// the "Add Instrument" PickList. Updated on Reload.
    instruments: Vec<String>,
    /// Index of the currently highlighted entry in `instruments`.
    /// Purely UI state (matches C++'s un-persisted `availableSelection`).
    selected_instrument: usize,
    /// Which top-level page is on screen. Ephemeral — resets to `Main`
    /// every time the editor is opened. Row param values persist via
    /// nih-plug regardless of tab visibility, so this doesn't hide data.
    active_tab: Tab,
    /// Mirror of `track_label_storage`'s current string. Lives here
    /// (rather than lensing directly through the Arc/RwLock) because
    /// Vizia's reactive binding wants a plain `String` lens target.
    track_label: String,
    /// Persistent backing store, shared with `CComidiParams::track_label`
    /// so edits land in the `#[persist]`-serialized form.
    track_label_storage: Arc<RwLock<String>>,
}

impl Model for Data {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|app_event: &AppEvent, _| match app_event {
            AppEvent::ReloadVoicegroup => {
                let state = load_voicegroup();
                self.vg_status = format_status(&state);
                self.slots = state.slots;
                self.instruments = state.available_instruments;
                // Keep the selection in range after a list refresh; 0 is
                // a safe fallback even if the list is empty (the PickList
                // won't dereference it until the user opens the popup).
                if self.selected_instrument >= self.instruments.len() {
                    self.selected_instrument = 0;
                }
            }
            AppEvent::SelectInstrument(idx) => {
                self.selected_instrument = *idx;
                // The 14-bit CC pair can only address 0..=16383. Larger
                // indices are silently dropped — the audio thread also
                // re-validates, so this is belt-and-suspenders.
                if (*idx as u32) <= crate::voicegroup::MAX_INSTRUMENT_INDEX {
                    // `Release` pairs with the audio thread's `AcqRel`
                    // swap; nothing on the UI side needs synchronizing
                    // *before* this store, so a plain release is enough.
                    self.pending_add_instrument
                        .store(*idx as i32, Ordering::Release);
                }
            }
            AppEvent::SwitchTab(tab) => {
                self.active_tab = *tab;
            }
            AppEvent::SetTrackLabel(s) => {
                // Write-through: update the mirrored string that the
                // lens reads from, then push the same value into the
                // persistent storage so nih-plug serializes it next
                // time the host saves project/preset state. `write()`
                // can only fail if another thread panicked while
                // holding the lock — treat that as fatal.
                self.track_label = s.clone();
                *self.track_label_storage.write().unwrap() = s.clone();
            }
        });
    }
}

pub(crate) fn default_state() -> Arc<ViziaState> {
    // Height grown another 60px to seat the tab bar above the content.
    // The Additional Commands page is shorter than Main, so this size
    // is driven by Main's content + tabs.
    ViziaState::new_with_default_scale_factor(|| (600, 620), 1.25)
}

pub(crate) fn create(
    params: Arc<CComidiParams>,
    editor_state: Arc<ViziaState>,
    pending_add_instrument: Arc<AtomicI32>,
) -> Option<Box<dyn Editor>> {
    create_vizia_editor(editor_state, ViziaTheming::Custom, move |cx, _| {
        cx.add_font_mem(include_bytes!("../Calamity-Regular.otf"));
        assets::register_noto_sans_light(cx);
        assets::register_noto_sans_thin(cx);
        // Best-effort stylesheet load; if Vizia rejects the CSS the UI
        // still works, it just won't animate.
        let _ = cx.add_stylesheet(INTERACTION_CSS);

        // Synchronous initial load so the status line + instrument list
        // are populated before the user ever touches Reload.
        let initial_state = load_voicegroup();
        let initial_status = format_status(&initial_state);

        // Seed the mirrored label from whatever nih-plug restored from
        // project state before `editor()` was called. If the lock is
        // poisoned we fall back to an empty string rather than panic —
        // losing a label is less bad than failing to open the editor.
        let initial_label = params
            .track_label
            .read()
            .map(|s| s.clone())
            .unwrap_or_default();
        let track_label_storage = params.track_label.clone();

        Data {
            params: params.clone(),
            pending_add_instrument: pending_add_instrument.clone(),
            vg_status: initial_status,
            slots: initial_state.slots,
            instruments: initial_state.available_instruments,
            selected_instrument: 0,
            active_tab: Tab::Main,
            track_label: initial_label,
            track_label_storage,
        }
        .build(cx);

        VStack::new(cx, |cx| {
            title_row(cx);

            tab_bar(cx);

            // `Binding` rebuilds its children whenever the lens value
            // changes — so switching tabs destroys one page's widget
            // tree and builds the other's. Param values live on the
            // params struct (not in widgets), so nothing is lost when
            // a tab swaps out: switching back rebinds the widgets to
            // the same underlying state.
            Binding::new(cx, Data::active_tab, |cx, tab| match tab.get(cx) {
                Tab::Main => main_tab_content(cx),
                Tab::Additional => additional_tab_content(cx),
            });
        })
        .font_family(vec![FamilyOwned::Name(String::from("Calamity"))])
        // Rhythm: 8px between stacked rows, 12px inset on top/bottom, 4px
        // on the sides so rows line up with the window edge through the
        // inner `child_left` values.
        .row_between(Pixels(8.0))
        .child_top(Pixels(12.0))
        .child_bottom(Pixels(12.0));
    })
}

// -----------------------------------------------------------------------------
// Title row: plugin name + user-entered track label
// -----------------------------------------------------------------------------

/// Header strip: the "ccomidi" wordmark on the left, a free-form
/// "Track label" textbox on the right.
///
/// Why a manual textbox and not a CLAP-provided track name: see
/// CLAUDE.md — the host-provided `clap.track-info/1` isn't wrapped by
/// nih-plug and only Bitwig/Reaper implement it anyway. A persisted
/// user string works in every DAW and survives project reloads.
fn title_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        Label::new(cx, "ccomidi")
            .font_size(24.0)
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

        // `on_submit` fires on Enter or when the textbox loses focus,
        // which is what you'd expect from a "name a thing" field. The
        // `bool` arg of the callback is whether Enter was pressed — we
        // don't care, either path should commit.
        Textbox::new(cx, Data::track_label)
            .on_submit(|cx, value, _| {
                cx.emit(AppEvent::SetTrackLabel(value));
            })
            .width(Pixels(240.0))
            .height(Pixels(26.0))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));
    })
    .col_between(Pixels(12.0))
    .height(Pixels(40.0))
    .child_left(Pixels(16.0))
    .child_right(Pixels(16.0));
}

// -----------------------------------------------------------------------------
// Top-level tab bar
// -----------------------------------------------------------------------------

fn tab_bar(cx: &mut Context) {
    HStack::new(cx, |cx| {
        tab_button(cx, "Main", Tab::Main, 120.0);
        tab_button(cx, "Additional Commands", Tab::Additional, 200.0);
    })
    .height(Pixels(38.0))
    .col_between(Pixels(4.0))
    .child_left(Pixels(16.0))
    .child_top(Pixels(4.0))
    .child_bottom(Pixels(4.0));
}

fn tab_button(cx: &mut Context, label: &str, tab: Tab, width: f32) {
    let owned_label = label.to_string();
    Button::new(
        cx,
        move |cx| cx.emit(AppEvent::SwitchTab(tab)),
        move |cx| {
            Label::new(cx, owned_label.as_str())
                .font_size(12.0)
                .color(Color::white())
                .child_space(Stretch(1.0))
        },
    )
    .width(Pixels(width))
    .height(Pixels(30.0))
    .border_radius(Pixels(6.0))
    .cursor(CursorIcon::Hand)
    .class("tap")
    .background_color(Data::active_tab.map(move |t| {
        if *t == tab {
            Color::rgb(110, 140, 220)
        } else {
            Color::rgb(60, 60, 70)
        }
    }));
}

// -----------------------------------------------------------------------------
// Tab content
// -----------------------------------------------------------------------------

/// Everything under the Main tab — the routing + fixed-row + voicegroup page.
fn main_tab_content(cx: &mut Context) {
    transport_row(cx);

    section_header(cx, "Fixed commands");
    for i in 0..FIXED_ROWS {
        // Pan is special-cased: for Square/Noise/ProgWave hardware
        // channels it's a 3-way enumeration (L/C/R → 0/64/127), for
        // DirectSound (and anything else we don't recognize) it's the
        // normal continuous slider. See `pan_row` for the dispatch.
        if i == 1 {
            pan_row(cx);
        } else {
            fixed_row(cx, i);
        }
    }

    section_header(cx, "Voicegroup");
    voicegroup_row(cx);
    add_instrument_row(cx);
}

/// The Additional Commands tab: a table of the freely-assignable rows.
/// Row count comes from `DYN_ROWS` (= `MAX_ROWS - FIXED_ROW_COUNT`).
fn additional_tab_content(cx: &mut Context) {
    section_header(cx, "Additional Commands");
    dynamic_header(cx);
    for i in 0..DYN_ROWS {
        dynamic_row(cx, i);
    }
}

/// Transport area: channel radio row on top, program controls below.
fn transport_row(cx: &mut Context) {
    channel_radio_row(cx);
    program_row(cx);
}

/// 16 radio-style buttons, one per MIDI channel (1..=16 display, 0..=15 wire).
/// Clicking a button normalizes the channel index and emits the three-event
/// param-change sequence nih-plug expects from custom widgets.
fn channel_radio_row(cx: &mut Context) {
    VStack::new(cx, |cx| {
        // Caption above, centered horizontally. `child_space(Stretch)` puts
        // stretch on both sides of the label text, which centers it inside
        // the label's own (full-width) box.
        Label::new(cx, "Output channel")
            .font_size(11.0)
            .height(Pixels(16.0))
            .child_space(Stretch(1.0));

        // Button row, also centered. `child_left/right(Stretch)` on the
        // HStack pushes stretch-space to the sides while the 16 buttons
        // keep their intrinsic widths — no row-fill stretching between
        // them.
        HStack::new(cx, |cx| {
            for ch in 0u8..16 {
                channel_radio_button(cx, ch);
            }
        })
        .col_between(Pixels(3.0))
        .height(Pixels(32.0))
        .child_left(Stretch(1.0))
        .child_right(Stretch(1.0));
    })
    .row_between(Pixels(4.0))
    .height(Pixels(52.0));
}

/// One of the 16 channel buttons.
///
/// # Custom param write
///
/// `ParamSlider`/`ParamButton` encapsulate the param-update protocol, but
/// for a plain `Button` we emit the trio by hand:
///   1. `BeginSetParameter`         — tells the host "user is editing"
///   2. `SetParameterNormalized`    — the actual new [0.0, 1.0] value
///   3. `EndSetParameter`           — host stops recording automation
///
/// `preview_normalized(plain)` is nih-plug's helper that maps `plain` (an
/// `i32` in the param's range) to the normalized form the host expects.
fn channel_radio_button(cx: &mut Context, ch: u8) {
    let ch_i = ch as i32;
    let label = format!("{}", ch + 1); // 1-indexed for UX

    Button::new(
        cx,
        move |cx| {
            let params = Data::params.get(cx);
            let ptr = params.channel.as_ptr();
            let normalized = params.channel.preview_normalized(ch_i);
            cx.emit(RawParamEvent::BeginSetParameter(ptr));
            cx.emit(RawParamEvent::SetParameterNormalized(ptr, normalized));
            cx.emit(RawParamEvent::EndSetParameter(ptr));
        },
        move |cx| {
            // `child_space(Stretch(1.0))` centers the number horizontally
            // and vertically inside the button. White text reads equally
            // well on the blue "selected" fill and the dark-gray "idle"
            // fill.
            Label::new(cx, label.as_str())
                .font_size(10.0)
                .color(Color::white())
                .child_space(Stretch(1.0))
        },
    )
    // 32×26 → more generous hit area than 28×22 without blowing out the
    // row width (16 × 32 + 15 × 3 = 557, inside the 600-wide window).
    .width(Pixels(32.0))
    .height(Pixels(26.0))
    // Concentric radius: buttons sit inside a parent that has no radius,
    // so the button radius is free to pick up shape on its own. 5px reads
    // as "pill-ish square" and matches the `bool_toggle` below.
    .border_radius(Pixels(5.0))
    // Pointing-hand cursor on hover signals the button is clickable,
    // consistent with web / macOS native button expectations.
    .cursor(CursorIcon::Hand)
    // Tactile scale-down on press — see the `tap` rule in INTERACTION_CSS.
    .class("tap")
    // Selected button gets a distinct fill. `.map(…)` on a lens produces a
    // new lens whose target is the mapped value — so this cell's background
    // automatically re-renders when the channel param changes from any
    // source (UI, host automation, preset recall, …).
    .background_color(Data::params.map(move |p| {
        if p.channel.value() == ch_i {
            Color::rgb(110, 140, 220)
        } else {
            Color::rgb(60, 60, 70)
        }
    }));
}

/// Program-enable toggle + Program-number slider, side by side.
///
/// "Program On" is now rendered as a tiny colored rectangle next to a
/// plain text label — no longer a fat button with the label baked in.
fn program_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        HStack::new(cx, |cx| {
            bool_toggle(cx, |p| &p.program_enabled);
            Label::new(cx, "Program On")
                .font_size(11.0)
                .child_top(Stretch(1.0))
                .child_bottom(Stretch(1.0));
        })
        .col_between(Pixels(8.0))
        .width(Pixels(130.0))
        .height(Pixels(32.0))
        .child_top(Stretch(1.0))
        .child_bottom(Stretch(1.0));

        labeled_control(cx, "Program", 180.0, |cx| {
            ParamSlider::new(cx, Data::params, |p| &p.program).width(Pixels(170.0));
        });
    })
    // 16px inner column gap + 16px left inset to match the channel row
    // and section headers (consistent rhythm down the left edge).
    .col_between(Pixels(16.0))
    .height(Pixels(54.0))
    .child_left(Pixels(16.0));
}

/// Voicegroup line 1: status text + Reload button.
fn voicegroup_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        // `Label::new(cx, Data::vg_status)` subscribes the label to the
        // lens; any `self.vg_status = …` in `Model::event` auto-updates
        // the displayed text.
        Label::new(cx, Data::vg_status)
            .font_size(11.0)
            .width(Pixels(380.0))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

        Button::new(
            cx,
            |cx| cx.emit(AppEvent::ReloadVoicegroup),
            |cx| Label::new(cx, "Reload"),
        )
        .width(Pixels(80.0))
        .cursor(CursorIcon::Hand)
        .class("tap");
    })
    .col_between(Pixels(8.0))
    .height(Pixels(26.0))
    .child_left(Pixels(12.0));
}

/// Voicegroup line 2: instrument PickList — picking an entry immediately
/// fires the Add-Instrument CC pair (matching C++ "select = send" UX,
/// no separate Add button needed).
fn add_instrument_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        Label::new(cx, "Add instrument")
            .font_size(11.0)
            .width(Pixels(110.0))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

        // `PickList::new(cx, items_lens, selected_lens, show_chevron)`
        // renders the currently-selected string as the collapsed view,
        // and a scrollable list of all entries as the popup.
        PickList::new(cx, Data::instruments, Data::selected_instrument, true)
            .width(Pixels(360.0))
            .cursor(CursorIcon::Hand)
            .on_select(|cx, idx| {
                cx.emit(AppEvent::SelectInstrument(idx));
            });
    })
    .col_between(Pixels(8.0))
    .height(Pixels(32.0))
    .child_left(Pixels(16.0));
}

/// Small layout helper: caption above a control.
fn labeled_control<F>(cx: &mut Context, label: &str, width: f32, content: F)
where
    F: FnOnce(&mut Context),
{
    VStack::new(cx, |cx| {
        Label::new(cx, label).font_size(11.0).height(Pixels(16.0));
        content(cx);
    })
    .width(Pixels(width))
    .row_between(Pixels(4.0));
}

fn section_header(cx: &mut Context, title: &str) {
    Label::new(cx, title)
        .font_size(13.0)
        // Consistent 28px-tall header row with 10px of top padding —
        // the label sits with rhythm above the content it labels.
        .height(Pixels(28.0))
        .child_top(Pixels(10.0))
        .child_left(Pixels(16.0));
}

/// Pan row — variant of `fixed_row` that swaps the slider for a 3-way
/// L/C/R segmented control when the program number points at a GBA
/// hardware channel (Square, Noise, ProgWave). For DirectSound and
/// anything unrecognized we keep the normal continuous slider.
///
/// The underlying parameter stays an `IntParam(0..=127)` — we just pick
/// a different widget. Buttons write 0, 64, or 127 to it.
///
/// # Reactivity
///
/// Two nested `Binding`s trigger a rebuild of the right-hand widget:
/// - outer watches `Data::slots` (voicegroup reload)
/// - inner watches the `program` param value
///
/// Either change can flip the loaded instrument kind, so both need to
/// invalidate the subtree. Rebuild cost is negligible — a handful of
/// widgets.
fn pan_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        bool_toggle(cx, |p| &p.fixed_rows[1].enabled);

        Label::new(cx, FIXED_ROW_LABELS[1])
            .font_size(11.0)
            .width(Pixels(90.0))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

        Binding::new(cx, Data::slots, |cx, _| {
            Binding::new(cx, Data::params.map(|p| p.program.value()), |cx, _| {
                let slots = Data::slots.get(cx);
                let program = Data::params.get(cx).program.value() as u8;
                let kind = voicegroup::kind_for_program(&slots, program);
                if kind.is_enum_pan() {
                    three_way_pan(cx);
                } else {
                    ParamSlider::new(cx, Data::params, |p| &p.fixed_rows[1].value)
                        .width(Pixels(400.0));
                }
            });
        });
    })
    .col_between(Pixels(8.0))
    .height(Pixels(28.0))
    .child_left(Pixels(16.0));
}

/// The 3-button L / C / R segmented control shown in place of the Pan
/// slider for hardware-channel instruments. Each button writes the
/// corresponding MIDI CC value (0 / 64 / 127) to the same underlying
/// `fixed_rows[1].value` param the slider uses, so automation and the
/// emitted CC stream are identical in both modes.
fn three_way_pan(cx: &mut Context) {
    HStack::new(cx, |cx| {
        pan_choice_button(cx, "L", 0);
        pan_choice_button(cx, "C", 64);
        pan_choice_button(cx, "R", 127);
    })
    .col_between(Pixels(4.0))
    .width(Pixels(400.0))
    .height(Pixels(26.0));
}

/// One of the three Pan choices. Looks and behaves like a
/// `channel_radio_button` — selected state is lens-bound to the param
/// value, so host automation, preset recall, and slider-mode writes
/// all repaint it correctly.
fn pan_choice_button(cx: &mut Context, label: &str, value: i32) {
    let owned_label = label.to_string();

    Button::new(
        cx,
        move |cx| {
            // Same three-event handshake as `channel_radio_button`:
            // Begin / SetNormalized / End is the contract nih-plug's
            // host wrapper expects from custom param widgets.
            let params = Data::params.get(cx);
            let ptr = params.fixed_rows[1].value.as_ptr();
            let normalized = params.fixed_rows[1].value.preview_normalized(value);
            cx.emit(RawParamEvent::BeginSetParameter(ptr));
            cx.emit(RawParamEvent::SetParameterNormalized(ptr, normalized));
            cx.emit(RawParamEvent::EndSetParameter(ptr));
        },
        move |cx| {
            Label::new(cx, owned_label.as_str())
                .font_size(12.0)
                .color(Color::white())
                .child_space(Stretch(1.0))
        },
    )
    // Three buttons share a 400px row; 130 + 4 + 130 + 4 + 130 = 398 —
    // leaves a hair for the col_between gaps to absorb without layout
    // overflow.
    .width(Pixels(130.0))
    .height(Pixels(26.0))
    .border_radius(Pixels(5.0))
    .cursor(CursorIcon::Hand)
    .class("tap")
    .background_color(Data::params.map(move |p| {
        if p.fixed_rows[1].value.value() == value {
            Color::rgb(110, 140, 220)
        } else {
            Color::rgb(60, 60, 70)
        }
    }));
}

/// A fixed-row: toggle (colored square) + text label + value slider.
fn fixed_row(cx: &mut Context, i: usize) {
    HStack::new(cx, |cx| {
        bool_toggle(cx, move |p| &p.fixed_rows[i].enabled);

        Label::new(cx, FIXED_ROW_LABELS[i])
            .font_size(11.0)
            .width(Pixels(90.0))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

        ParamSlider::new(cx, Data::params, move |p| &p.fixed_rows[i].value).width(Pixels(400.0));
    })
    .col_between(Pixels(8.0))
    // 28px row height to match the channel row, giving the whole table a
    // uniform vertical cadence.
    .height(Pixels(28.0))
    .child_left(Pixels(16.0));
}

/// Tiny colored-square toggle bound to a [`BoolParam`].
///
/// # Why not `ParamButton`?
///
/// `ParamButton` renders the param's display name as its on-button text.
/// We want the label on the outside — so we roll our own:
///
/// - The visible square is ~32×22 and changes color based on state
///   (blue when `true`, dark when `false`). The color is lens-bound, so
///   it repaints automatically when the param changes from any source.
/// - A pointing-hand cursor on hover signals it's clickable.
/// - Clicking emits the standard nih-plug param-update trio the host
///   expects (Begin / SetNormalized / End).
fn bool_toggle<F>(cx: &mut Context, getter: F)
where
    F: Fn(&CComidiParams) -> &BoolParam + Copy + Send + Sync + 'static,
{
    Button::new(
        cx,
        move |cx| {
            // Snapshot the Arc, pull out the specific `BoolParam`, and
            // flip its value through the nih-plug event path.
            let params = Data::params.get(cx);
            let param = getter(params.as_ref());
            let ptr = param.as_ptr();
            let normalized = if param.value() { 0.0 } else { 1.0 };
            cx.emit(RawParamEvent::BeginSetParameter(ptr));
            cx.emit(RawParamEvent::SetParameterNormalized(ptr, normalized));
            cx.emit(RawParamEvent::EndSetParameter(ptr));
        },
        // Intentional: empty body — no label inside the button. The caller
        // places a `Label` next to the toggle instead.
        |cx| Label::new(cx, ""),
    )
    .width(Pixels(32.0))
    .height(Pixels(22.0))
    .border_radius(Pixels(5.0))
    .cursor(CursorIcon::Hand)
    .class("tap")
    .background_color(Data::params.map(move |p| {
        if getter(p).value() {
            Color::rgb(110, 140, 220)
        } else {
            Color::rgb(60, 60, 70)
        }
    }));
}

// -----------------------------------------------------------------------------
// Helpers used by Model::event — keep I/O-capable ones on the UI thread only.
// -----------------------------------------------------------------------------

fn load_voicegroup() -> VoicegroupState {
    match voicegroup::resolve_state_path() {
        Some(path) => voicegroup::load_state(&path),
        None => VoicegroupState {
            error: Some("could not locate plugin bundle".into()),
            ..Default::default()
        },
    }
}

/// Condense the loaded state into a one-line status message. Errors win
/// over success info so misconfiguration stands out.
fn format_status(state: &VoicegroupState) -> String {
    if let Some(err) = &state.error {
        if state.available_instruments.is_empty() {
            return format!("⚠ {err}");
        }
        return format!(
            "⚠ {}  ({} instruments available)",
            err,
            state.available_instruments.len()
        );
    }
    format!(
        "{} slots · {} instruments available",
        state.slots.len(),
        state.available_instruments.len()
    )
}

// -----------------------------------------------------------------------------
// Dynamic row helpers — hidden while diagnosing lag, kept to re-enable easily.
// -----------------------------------------------------------------------------

/// Caption row above the dynamic table.
fn dynamic_header(cx: &mut Context) {
    HStack::new(cx, |cx| {
        Label::new(cx, "On").font_size(10.0).width(Pixels(32.0));
        Label::new(cx, "Command").font_size(10.0).width(Pixels(180.0));
        for field in ["f0", "f1", "f2", "f3"] {
            Label::new(cx, field).font_size(10.0).width(Pixels(72.0));
        }
    })
    .col_between(Pixels(6.0))
    .height(Pixels(16.0))
    .child_left(Pixels(16.0));
}

/// One configurable row in the Additional Commands table.
///
/// Sizes are tuned for the 600-wide window:
///   32 (toggle) + 180 (cmd) + 4 × 72 (fields) + 5 × 6 (gaps) + 16 (left)
///   = 546 px, well inside the window.
fn dynamic_row(cx: &mut Context, i: usize) {
    HStack::new(cx, |cx| {
        bool_toggle(cx, move |p| &p.dyn_rows[i].enabled);
        // ParamSlider over an EnumParam cycles through variants — not
        // the prettiest widget for long enum lists, but avoids the cost
        // of per-row PickLists.
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].cmd).width(Pixels(180.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f0).width(Pixels(72.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f1).width(Pixels(72.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f2).width(Pixels(72.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f3).width(Pixels(72.0));
    })
    .col_between(Pixels(6.0))
    .height(Pixels(28.0))
    .child_left(Pixels(16.0));
}
