//! Vizia editor — the plugin window the host shows when the user opens
//! ccomidi's UI.
//!
//! # Layout (top → bottom)
//!
//!   1. Title row
//!   2. Transport row: MIDI channel, Program-enable toggle, Program number
//!   3. "Fixed commands" section: Volume / Pan / Mod / LFO Speed (4 rows)
//!   4. "Dynamic commands" section: 12 freely-assignable rows
//!
//! # Vizia idioms used here
//!
//! - `#[derive(Lens)]` on `Data` generates type-safe projectors so the UI
//!   can observe `params` reactively: `Data::params` is a *lens*, not a
//!   field access.
//! - `ParamSlider`/`ParamButton` take the root lens plus a `Fn(&Params)
//!   -> &Param` closure that picks out one leaf. The closure must be
//!   `'static + Copy` so we capture `i: usize` by `move` inside each loop.
//! - Layout is flexbox-ish: `VStack`/`HStack` position children along one
//!   axis, and `child_*` / `row_between` / `col_between` add padding.

use nih_plug::prelude::Editor;
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};
use std::sync::Arc;

use crate::params::{CComidiParams, DYN_ROWS, FIXED_ROWS};

/// Labels for the four fixed rows. Indexed 0..FIXED_ROWS.
const FIXED_ROW_LABELS: [&str; FIXED_ROWS] = ["Volume", "Pan", "Mod", "LFO Speed"];

/// Root Vizia model: everything the widget tree can observe lives here.
#[derive(Lens)]
struct Data {
    params: Arc<CComidiParams>,
}

impl Model for Data {}

pub(crate) fn default_state() -> Arc<ViziaState> {
    // Wide enough for four field sliders side-by-side on a dynamic row,
    // tall enough to show all 12 dynamic rows without scrolling.
    ViziaState::new(|| (820, 760))
}

pub(crate) fn create(
    params: Arc<CComidiParams>,
    editor_state: Arc<ViziaState>,
) -> Option<Box<dyn Editor>> {
    create_vizia_editor(editor_state, ViziaTheming::Custom, move |cx, _| {
        assets::register_noto_sans_light(cx);
        assets::register_noto_sans_thin(cx);

        Data {
            params: params.clone(),
        }
        .build(cx);

        VStack::new(cx, |cx| {
            // -- title ----------------------------------------------------
            Label::new(cx, "ccomidi")
                .font_size(28.0)
                .height(Pixels(44.0))
                .child_space(Stretch(1.0));

            // -- transport row --------------------------------------------
            transport_row(cx);

            // -- fixed commands -------------------------------------------
            section_header(cx, "Fixed commands");
            for i in 0..FIXED_ROWS {
                fixed_row(cx, i);
            }

            // -- dynamic commands -----------------------------------------
            section_header(cx, "Dynamic commands");
            dynamic_header(cx);
            for i in 0..DYN_ROWS {
                dynamic_row(cx, i);
            }
        })
        .row_between(Pixels(4.0))
        .child_top(Pixels(8.0))
        .child_bottom(Pixels(8.0));
    })
}

/// Channel + Program-enable + Program, all in one row.
fn transport_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        labeled_control(cx, "Channel", 140.0, |cx| {
            ParamSlider::new(cx, Data::params, |p| &p.channel).width(Pixels(120.0));
        });
        labeled_control(cx, "Program Enable", 140.0, |cx| {
            ParamButton::new(cx, Data::params, |p| &p.program_enabled);
        });
        labeled_control(cx, "Program", 200.0, |cx| {
            ParamSlider::new(cx, Data::params, |p| &p.program).width(Pixels(180.0));
        });
    })
    .col_between(Pixels(20.0))
    .height(Pixels(62.0))
    .child_left(Pixels(16.0));
}

/// Small layout helper: a column with a caption above the actual control.
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
        .font_size(14.0)
        .height(Pixels(28.0))
        .child_top(Pixels(8.0))
        .child_left(Pixels(16.0));
}

/// A fixed-row: enable toggle + command label + single value slider.
fn fixed_row(cx: &mut Context, i: usize) {
    HStack::new(cx, |cx| {
        ParamButton::new(cx, Data::params, move |p| &p.fixed_rows[i].enabled).width(Pixels(60.0));

        Label::new(cx, FIXED_ROW_LABELS[i])
            .font_size(12.0)
            .width(Pixels(120.0))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

        ParamSlider::new(cx, Data::params, move |p| &p.fixed_rows[i].value).width(Pixels(560.0));
    })
    .col_between(Pixels(10.0))
    .height(Pixels(28.0))
    .child_left(Pixels(16.0));
}

/// Column captions for the dynamic table, shown once above row 0.
fn dynamic_header(cx: &mut Context) {
    HStack::new(cx, |cx| {
        Label::new(cx, "On").font_size(10.0).width(Pixels(60.0));
        Label::new(cx, "Command").font_size(10.0).width(Pixels(220.0));
        for field in ["f0", "f1", "f2", "f3"] {
            Label::new(cx, field).font_size(10.0).width(Pixels(96.0));
        }
    })
    .col_between(Pixels(8.0))
    .height(Pixels(16.0))
    .child_left(Pixels(16.0));
}

/// A dynamic row: enable + command dropdown + four field sliders.
fn dynamic_row(cx: &mut Context, i: usize) {
    HStack::new(cx, |cx| {
        ParamButton::new(cx, Data::params, move |p| &p.dyn_rows[i].enabled).width(Pixels(60.0));

        // ParamSlider over an EnumParam renders as a cycling selector.
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].cmd).width(Pixels(220.0));

        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f0).width(Pixels(96.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f1).width(Pixels(96.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f2).width(Pixels(96.0));
        ParamSlider::new(cx, Data::params, move |p| &p.dyn_rows[i].f3).width(Pixels(96.0));
    })
    .col_between(Pixels(8.0))
    .height(Pixels(26.0))
    .child_left(Pixels(16.0));
}
