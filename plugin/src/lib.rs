use nih_plug::prelude::*;
use nih_plug_vizia::ViziaState;
use std::sync::Arc;

// Framework-agnostic MIDI command logic (ported from the C++ ccomidi project).
// Not yet wired into `process` — that happens in a later pass once we design
// the nih-plug parameter layout. Kept in the tree so `cargo test` exercises
// it and the build keeps it honest.
//
// `dead_code` + `unused_imports` are expected until phase 2 hooks this up.
#[allow(dead_code, unused_imports)]
mod core;

mod editor;

pub struct CComidiPlugin {
    params: Arc<CComidiParams>,
}

#[derive(Params)]
pub struct CComidiParams {
    #[persist = "editor-state"]
    editor_state: Arc<ViziaState>,

    #[id = "passthrough"]
    pub passthrough: BoolParam,
}

impl Default for CComidiPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(CComidiParams::default()),
        }
    }
}

impl Default for CComidiParams {
    fn default() -> Self {
        Self {
            editor_state: editor::default_state(),
            passthrough: BoolParam::new("MIDI Passthrough", true),
        }
    }
}

impl Plugin for CComidiPlugin {
    const NAME: &'static str = "ccomidi (NIH prototype)";
    const VENDOR: &'static str = "ccomidi";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // MIDI-only plugin: no audio channels required.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: None,
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::MidiCCs;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(self.params.clone(), self.params.editor_state.clone())
    }

    fn process(
        &mut self,
        _buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let passthrough = self.params.passthrough.value();
        while let Some(event) = context.next_event() {
            if passthrough {
                context.send_event(event);
            }
        }
        ProcessStatus::Normal
    }
}

impl ClapPlugin for CComidiPlugin {
    const CLAP_ID: &'static str = "com.ccomidi.nih-prototype";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("NIH-plug + Vizia prototype for ccomidi");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] =
        &[ClapFeature::NoteEffect, ClapFeature::Utility];
}

nih_export_clap!(CComidiPlugin);
