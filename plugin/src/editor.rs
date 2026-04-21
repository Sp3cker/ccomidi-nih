use nih_plug::prelude::Editor;
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};
use std::sync::Arc;

use crate::CComidiParams;

#[derive(Lens)]
struct Data {
    params: Arc<CComidiParams>,
}

impl Model for Data {}

pub(crate) fn default_state() -> Arc<ViziaState> {
    ViziaState::new(|| (640, 360))
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
            Label::new(cx, "ccomidi")
                .font_size(32.0)
                .height(Pixels(50.0))
                .child_top(Stretch(1.0))
                .child_bottom(Pixels(0.0));

            Label::new(cx, "NIH-plug + Vizia prototype")
                .font_size(14.0)
                .height(Pixels(30.0));

            Label::new(cx, "MIDI Passthrough")
                .font_size(14.0)
                .top(Pixels(20.0));
            ParamButton::new(cx, Data::params, |p| &p.passthrough);
        })
        .row_between(Pixels(6.0))
        .child_left(Stretch(1.0))
        .child_right(Stretch(1.0));
    })
}
