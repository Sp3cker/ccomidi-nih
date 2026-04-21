//! Vizia editor — the plugin window the host shows when the user opens
//! ccomidi's UI.
//!
//! # Layout (top → bottom)
//!
//!   1. Title row
//!   2. Transport row: MIDI channel, Program-enable toggle, Program number
//!   3. "Fixed commands" section: Volume / Pan / Mod / LFO Speed (4 rows)
//!   4. "Voicegroup" section: status + Reload, index slider + Add button
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
//! - Non-param UI state (voicegroup status text) lives in `Data` as a
//!   plain `String`. It's mutated inside `Model::event` in response to
//!   `AppEvent`s emitted from button presses.

use nih_plug::prelude::{Editor, Param};
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

// DYN_ROWS is unused while the dynamic section is hidden, but keep the
// import commented so re-enabling is a one-line change.
use crate::params::{CComidiParams, FIXED_ROWS /*, DYN_ROWS */};
use crate::voicegroup::{self, VoicegroupState};

/// Messages the editor fires at itself.
///
/// - `ReloadVoicegroup` rereads `poryaaaa_state.json` and refreshes
///   `Data::vg_status`.
/// - `EmitAddInstrument` reads the current `add_instrument_index` param
///   and stores it into the shared `pending_add_instrument` atomic; the
///   audio thread picks it up on its next block and emits CC#98/#99.
#[derive(Debug)]
enum AppEvent {
    ReloadVoicegroup,
    EmitAddInstrument,
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
}

impl Model for Data {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|app_event: &AppEvent, _| match app_event {
            AppEvent::ReloadVoicegroup => {
                self.vg_status = format_status(&load_voicegroup());
            }
            AppEvent::EmitAddInstrument => {
                let idx = self.params.add_instrument_index.value();
                // `Release` pairs with the audio thread's `AcqRel` swap;
                // nothing on the UI side needs synchronizing *before* this
                // store, so a plain release is enough.
                self.pending_add_instrument.store(idx, Ordering::Release);
            }
        });
    }
}

pub(crate) fn default_state() -> Arc<ViziaState> {
    // 1.25 user scale carries over from the fixed-only window; logical
    // width widened again so all 16 channel buttons fit comfortably with
    // two-digit labels ("10".."16").
    ViziaState::new_with_default_scale_factor(|| (600, 360), 1.25)
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

        // Synchronous initial load so the status line shows something
        // useful before the user touches Reload.
        let initial_status = format_status(&load_voicegroup());

        Data {
            params: params.clone(),
            pending_add_instrument: pending_add_instrument.clone(),
            vg_status: initial_status,
        }
        .build(cx);

        VStack::new(cx, |cx| {
            Label::new(cx, "ccomidi")
                .font_size(24.0)
                .height(Pixels(36.0))
                .child_space(Stretch(1.0));

            transport_row(cx);

            section_header(cx, "Fixed commands");
            for i in 0..FIXED_ROWS {
                fixed_row(cx, i);
            }

            section_header(cx, "Voicegroup");
            voicegroup_row(cx);
            add_instrument_row(cx);

            // Dynamic commands section hidden while diagnosing GUI lag:
            //
            //   section_header(cx, "Dynamic commands");
            //   dynamic_header(cx);
            //   for i in 0..DYN_ROWS {
            //       dynamic_row(cx, i);
            //   }
        })
        .font_family(vec![FamilyOwned::Name(String::from("Calamity"))])
        .row_between(Pixels(4.0))
        .child_top(Pixels(8.0))
        .child_bottom(Pixels(8.0));
    })
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
    HStack::new(cx, |cx| {
        Label::new(cx, "Channel")
            .font_size(11.0)
            .width(Pixels(60.0))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

        for ch in 0u8..16 {
            channel_radio_button(cx, ch);
        }
    })
    .col_between(Pixels(2.0))
    .height(Pixels(28.0))
    .child_left(Pixels(12.0));
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
    // Wider than 22px so two-digit labels ("10".."16") don't clip.
    .width(Pixels(28.0))
    .height(Pixels(22.0))
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
fn program_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        labeled_control(cx, "Program On", 90.0, |cx| {
            ParamButton::new(cx, Data::params, |p| &p.program_enabled);
        });
        labeled_control(cx, "Program", 180.0, |cx| {
            ParamSlider::new(cx, Data::params, |p| &p.program).width(Pixels(170.0));
        });
    })
    .col_between(Pixels(12.0))
    .height(Pixels(50.0))
    .child_left(Pixels(12.0));
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
        .width(Pixels(80.0));
    })
    .col_between(Pixels(8.0))
    .height(Pixels(26.0))
    .child_left(Pixels(12.0));
}

/// Voicegroup line 2: index slider + Add Instrument button.
fn add_instrument_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        Label::new(cx, "Add Instrument #")
            .font_size(11.0)
            .width(Pixels(130.0))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

        ParamSlider::new(cx, Data::params, |p| &p.add_instrument_index).width(Pixels(260.0));

        Button::new(
            cx,
            |cx| cx.emit(AppEvent::EmitAddInstrument),
            |cx| Label::new(cx, "Add"),
        )
        .width(Pixels(70.0));
    })
    .col_between(Pixels(8.0))
    .height(Pixels(26.0))
    .child_left(Pixels(12.0));
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
        .height(Pixels(22.0))
        .child_top(Pixels(6.0))
        .child_left(Pixels(12.0));
}

/// A fixed-row: enable toggle (whose label is the row name) + value slider.
fn fixed_row(cx: &mut Context, i: usize) {
    HStack::new(cx, |cx| {
        ParamButton::new(cx, Data::params, move |p| &p.fixed_rows[i].enabled)
            .width(Pixels(110.0));

        ParamSlider::new(cx, Data::params, move |p| &p.fixed_rows[i].value).width(Pixels(370.0));
    })
    .col_between(Pixels(8.0))
    .height(Pixels(26.0))
    .child_left(Pixels(12.0));
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

#[allow(dead_code)]
fn dynamic_header(cx: &mut Context) {
    HStack::new(cx, |cx| {
        Label::new(cx, "Row").font_size(10.0).width(Pixels(100.0));
        Label::new(cx, "Command").font_size(10.0).width(Pixels(200.0));
        for field in ["f0", "f1", "f2", "f3"] {
            Label::new(cx, field).font_size(10.0).width(Pixels(96.0));
        }
    })
    .col_between(Pixels(8.0))
    .height(Pixels(16.0))
    .child_left(Pixels(16.0));
}

#[allow(dead_code)]
fn dynamic_row(cx: &mut Context, i: usize) {
    HStack::new(cx, |cx| {
        ParamButton::new(cx, Data::params, move |p| &p.dyn_rows[i].enabled)
            .width(Pixels(100.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].cmd).width(Pixels(200.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f0).width(Pixels(96.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f1).width(Pixels(96.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f2).width(Pixels(96.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f3).width(Pixels(96.0));
    })
    .col_between(Pixels(8.0))
    .height(Pixels(26.0))
    .child_left(Pixels(16.0));
}
