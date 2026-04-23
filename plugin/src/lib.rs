use nih_plug::prelude::*;
use std::sync::Arc;

// Some `SenderCore` helpers (getters, `reset`, etc.) are legitimate public
// API that the plugin doesn't currently call — they'll be used once we
// implement diff-on-param-change emission.
#[allow(dead_code)]
mod core;
mod editor;
mod params;
pub(crate) mod voicegroup;

use params::CComidiParams;

pub struct CComidiPlugin {
    /// Host-synchronized parameter block (see `params.rs`). `Arc` because
    /// the editor thread and audio thread both hold a handle; nih-plug's
    /// params use atomics internally so shared read/write is safe.
    params: Arc<CComidiParams>,

    /// Framework-agnostic emission engine. Owned by the plugin because it
    /// carries mutable per-block state (last-transport-playing, etc.) that
    /// only the audio thread touches.
    sender: core::SenderCore,
}

impl Default for CComidiPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(CComidiParams::default()),
            sender: core::SenderCore::new(),
        }
    }
}

impl CComidiPlugin {
    /// Copy current nih-plug parameter values into [`core::SenderCore`]
    /// state. Called once per audio block, before we tick the sender.
    ///
    /// # Why a sync step instead of reading params from inside core?
    ///
    /// Keeping core free of nih-plug types means unit tests don't need the
    /// whole plugin infrastructure. The "cost" is this plain-data copy —
    /// but every setter is a no-op if the value didn't change, and the
    /// entire block (3 + 4*2 + 12*6 = 83 setter calls) is stack-only work
    /// measured in nanoseconds.
    fn sync_params_to_core(&mut self) {
        let p = &*self.params;

        self.sender.set_channel(p.channel.value() as u8);
        self.sender.set_program(p.program.value() as u8);
        self.sender.set_program_enabled(p.program_enabled.value());

        // Fixed rows: only value[0] is meaningful.
        for i in 0..params::FIXED_ROWS {
            let r = &p.fixed_rows[i];
            self.sender.set_row_enabled(i, r.enabled.value());
            self.sender.set_row_field(i, 0, r.value.value() as u8);
        }

        // Dynamic rows: cmd + four fields.
        for i in 0..params::DYN_ROWS {
            let r = &p.dyn_rows[i];
            let row_idx = params::FIXED_ROWS + i;
            self.sender.set_row_enabled(row_idx, r.enabled.value());
            self.sender.set_row_cmd(row_idx, r.cmd.value().into());
            self.sender.set_row_field(row_idx, 0, r.f0.value() as u8);
            self.sender.set_row_field(row_idx, 1, r.f1.value() as u8);
            self.sender.set_row_field(row_idx, 2, r.f2.value() as u8);
            self.sender.set_row_field(row_idx, 3, r.f3.value() as u8);
        }
    }
}

/// Adapter that lets [`core::SenderCore`] write events into nih-plug's
/// per-block `ProcessContext` without core itself depending on nih-plug.
///
/// # Lifetime / generics, briefly
///
/// `'a` is the lifetime of the borrow into the context, and `C` is the
/// concrete context type the host gave us this block. Because we implement
/// the trait generically over `C`, the compiler monomorphizes one copy per
/// host (CLAP, VST3, standalone), so dispatch is a direct call — no vtable.
struct NihSink<'a, C: ProcessContext<CComidiPlugin>> {
    ctx: &'a mut C,
}

/// Rewrite an event's channel nibble in place.
///
/// Every channel-voice MIDI message (note on/off, CC, pitch bend, program
/// change, channel pressure, poly pressure) gets retargeted. Non-channel
/// events (tempo, MIDI clock, raw sysex, …) pass through untouched.
///
/// `match` covers enum variants structurally — using `|` to share the same
/// body across shapes that happen to carry the same fields lets the compiler
/// catch any variant we forget to handle.
fn retarget_channel<S>(ev: &mut NoteEvent<S>, new_channel: u8) {
    match ev {
        NoteEvent::NoteOn { channel, .. }
        | NoteEvent::NoteOff { channel, .. }
        | NoteEvent::Choke { channel, .. }
        | NoteEvent::PolyPressure { channel, .. }
        | NoteEvent::MidiChannelPressure { channel, .. }
        | NoteEvent::MidiPitchBend { channel, .. }
        | NoteEvent::MidiCC { channel, .. }
        | NoteEvent::MidiProgramChange { channel, .. } => {
            *channel = new_channel;
        }
        // Everything else — timing events, polyphonic-expression voice
        // terminators, parameter automation, etc. — has no channel to
        // rewrite. Leave untouched.
        _ => {}
    }
}

impl<'a, C: ProcessContext<CComidiPlugin>> core::EventSink for NihSink<'a, C> {
    fn push_cc(&mut self, timing: u32, channel: u8, cc: u8, value: u8) {
        // nih-plug wants the CC value as a 0.0..=1.0 float; we divide so
        // that `127 / 127` round-trips losslessly for any integer 0..=127.
        self.ctx.send_event(NoteEvent::MidiCC {
            timing,
            channel,
            cc,
            value: value as f32 / 127.0,
        });
    }

    fn push_program(&mut self, timing: u32, channel: u8, program: u8) {
        self.ctx.send_event(NoteEvent::MidiProgramChange {
            timing,
            channel,
            program,
        });
    }
}

impl Plugin for CComidiPlugin {
    const NAME: &'static str = "ccomidi (NIH prototype)";
    const VENDOR: &'static str = "ccomidi";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // MIDI-only: we have no audio channels at all.
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

    /// Called by the host every time the plugin is (re-)activated. We clear
    /// the sender's *runtime* state (last-emitted caches etc.) so the next
    /// play-start triggers a fresh snapshot, while keeping the user's row
    /// configuration intact.
    fn reset(&mut self) {
        self.sender.reset_runtime();
    }

    fn process(
        &mut self,
        _buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // 1. Reflect host-facing param values into core state. Cheap.
        self.sync_params_to_core();

        // 2. Read transport state + target channel once at block start.
        //    `playing` flips on the sample the host starts the transport.
        let is_playing = context.transport().playing;
        let target_channel = self.params.channel.value() as u8;

        // 3. Pass-through every input MIDI event, but rewrite its channel
        //    nibble to the user-selected `target_channel`. Matches the C++
        //    ccomidi behavior in `src/plugin/ccomidi_plugin.cpp:705-728`:
        //    notes, pitch bend, CCs and everything else are re-sent on the
        //    target channel, so this plugin acts as a channel-routing
        //    stage as well as a CC source.
        while let Some(mut ev) = context.next_event() {
            retarget_channel(&mut ev, target_channel);
            context.send_event(ev);
        }

        // 4. Tick the sender. Borrows `context` mutably via NihSink —
        //    which is why the pass-through block above had to run first.
        let mut sink = NihSink { ctx: context };
        self.sender.tick(is_playing, &mut sink);

        ProcessStatus::Normal
    }
}

impl Vst3Plugin for CComidiPlugin {
    // Stable 16-byte class ID. Hand-picked ASCII sentinel so the plugin
    // presents a deterministic TUID across rebuilds; changing this
    // invalidates any host state already saved against the plugin.
    const VST3_CLASS_ID: [u8; 16] = *b"ccomidiNihV3Prot";
    // MIDI-only note FX utility. Bitwig groups Fx+Tools plugins into the
    // "Note FX" column of its VST3 browser.
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Tools];
}

nih_export_vst3!(CComidiPlugin);
