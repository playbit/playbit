//! A small piano demonstrating asynchronous output and overlapping PCM sounds.
#[cfg(not(target_arch = "wasm32"))]
extern crate playbit;

mod keyboard;
mod synthesis;
#[cfg(test)]
mod tests;

use keyboard::{Action, HEIGHT, Keyboard, NOTE_MS};
use playbit_api::abi::{
    input::{self, Input},
    syscall::{Error, EventKind, FrameInput},
};
use playbit_api::context::{
    Audio, Completion, Context, Gain, OperationHandle, Sound, SoundInstance, Surface, Timestamp,
};
use playbit_api::ui::{
    ButtonStyle, CanvasFrames, CanvasItem, CanvasRect, CanvasText, Color, TextStyle, ViewEncoder,
};

const CANVAS: u32 = 1;
const QUIETER: u32 = 10;
const LOUDER: u32 = 11;
const RETRY: u32 = 12;
const COLORS: [u32; 8] = [
    0xf64d47ff, 0xfa9438ff, 0xfcd440ff, 0x6cd959ff, 0x40ccccff, 0x4d8cffff, 0x9466f2ff, 0xf270bfff,
];

struct Piano {
    keyboard: Keyboard,
    pending: Option<OperationHandle>,
    audio: Option<Audio>,
    sounds: [Option<Sound>; 8],
    pcm: [Option<Vec<u8>>; 8],
    instances: [Option<SoundInstance>; 8],
    next_instance: usize,
    volume: u8,
    output_rate: u32,
    error: Option<String>,
}

impl Default for Piano {
    fn default() -> Self {
        Self {
            keyboard: Keyboard::default(),
            pending: None,
            audio: None,
            sounds: [None; 8],
            pcm: std::array::from_fn(|_| None),
            instances: [None; 8],
            next_instance: 0,
            volume: 70,
            output_rate: synthesis::SAMPLE_RATE,
            error: None,
        }
    }
}

impl Piano {
    fn ready(&self) -> bool {
        self.audio.is_some() && self.sounds.iter().all(Option::is_some)
    }

    fn open(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        self.pending = Some(context.begin_open_audio(synthesis::SAMPLE_RATE)?);
        self.error = None;
        Ok(())
    }

    fn complete(
        &mut self,
        context: &mut Context<'_>,
        operation: OperationHandle,
    ) -> Result<bool, Error> {
        if self.pending != Some(operation) {
            return Ok(false);
        }
        // Take only on the matching DataReady event. A terminal result consumes
        // the operation; a Pending result keeps it for the next ready event.
        match context.take_open_audio(operation) {
            Ok(Completion::Pending) => Ok(false),
            Ok(Completion::Ready(audio)) => {
                self.pending = None;
                self.audio = Some(audio);
                self.output_rate = context.audio_sample_rate(audio)?;
                context.audio_set_volume(audio, f32::from(self.volume) / 100.0)?;
                Ok(true)
            }
            Err(error) => {
                self.pending = None;
                Err(error)
            }
        }
    }

    fn prepare_note(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        let Some(audio) = self.audio else { return Ok(()) };
        let Some(index) = self.sounds.iter().position(Option::is_none) else { return Ok(()) };
        let pcm = self.pcm[index].get_or_insert_with(|| synthesis::note(index));
        self.sounds[index] = Some(context.sound_open_pcm(audio, 1, synthesis::SAMPLE_RATE, pcm)?);
        Ok(())
    }

    fn play(&mut self, context: &mut Context<'_>, index: usize) -> Result<(), Error> {
        if !self.ready() {
            return Ok(());
        }
        let slot = self.next_instance;
        if let Some(previous) = self.instances[slot] {
            context.sound_stop(previous)?;
            self.instances[slot] = None;
        }
        let playback = SoundInstance {
            sound: self.sounds[index].expect("ready sound bank"),
            instance: slot as u32,
        };
        context.sound_play(
            playback,
            Gain(65_536),
            Timestamp(0),
            Timestamp(synthesis::DURATION_NS),
            0,
        )?;
        self.instances[slot] = Some(playback);
        self.next_instance = (slot + 1) % self.instances.len();
        self.keyboard.lit[index] = NOTE_MS;
        self.error = None;
        Ok(())
    }

    fn change_volume(&mut self, context: &mut Context<'_>, delta: i32) -> Result<(), Error> {
        let volume = (i32::from(self.volume) + delta).clamp(0, 100) as u8;
        if volume != self.volume {
            if let Some(audio) = self.audio {
                context.audio_set_volume(audio, f32::from(volume) / 100.0)?;
            }
            self.volume = volume;
            if self.ready() {
                self.error = None;
            }
        }
        Ok(())
    }

    fn retry(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        if let Some(operation) = self.pending {
            context.dispose_operation(operation)?;
            self.pending = None;
        }
        if let Some(audio) = self.audio {
            context.audio_close(audio)?;
            self.audio = None;
        }
        self.sounds.fill(None);
        self.instances.fill(None);
        self.keyboard.lit.fill(0);
        self.next_instance = 0;
        self.open(context)
    }

    fn act(&mut self, context: &mut Context<'_>, action: Option<Action>) -> Result<(), Error> {
        match action {
            Some(Action::Play(index)) => self.play(context, index),
            Some(Action::Volume(delta)) => self.change_volume(context, delta),
            None => Ok(()),
        }
    }

    fn render(&self, context: &mut Context<'_>, view: &mut ViewEncoder<'_>) -> Result<(), Error> {
        view.window_input(input::POINTER | input::KEYBOARD);
        view.column_start();
        view.text(TextStyle::Title, "Audio Piano");
        view.text(
            TextStyle::Body,
            "Click or drag across the keys. ← → select a note; Space plays it.",
        );
        view.row_start();
        view.button(QUIETER, "−", ButtonStyle::Default, self.volume == 0);
        view.text(TextStyle::Body, &format!("Volume {}%", self.volume));
        view.button(LOUDER, "+", ButtonStyle::Default, self.volume == 100);
        view.row_end();
        let status = if let Some(error) = &self.error {
            format!("Sound unavailable: {error}")
        } else if self.pending.is_some() {
            "Opening audio output…".into()
        } else if self.ready() {
            format!("Ready · {} Hz output · up to eight overlapping notes", self.output_rate)
        } else {
            format!("Preparing notes ({}/8)…", self.sounds.iter().flatten().count())
        };
        view.text(TextStyle::Muted, &status);
        if self.error.is_some() && self.pending.is_none() {
            view.button(RETRY, "Retry audio", ButtonStyle::Default, false);
        }
        let mut drawing = vec![CanvasItem::Rect(CanvasRect {
            x: 0,
            y: 0,
            width: self.keyboard.width,
            height: HEIGHT,
            color: Color(0x171a20ff),
        })];
        for (index, name) in synthesis::NAMES.into_iter().enumerate() {
            let pad = self.keyboard.pad(index);
            drawing.push(CanvasItem::Rect(CanvasRect {
                color: Color(if index == self.keyboard.selected { 0xf2f4f8ff } else { 0x454b58ff }),
                ..pad
            }));
            let color = if self.keyboard.lit[index] > 0 { COLORS[index] } else { 0x303541ff };
            drawing.push(CanvasItem::Rect(CanvasRect {
                x: pad.x + 2,
                y: pad.y + 2,
                width: pad.width - 4,
                height: pad.height - 4,
                color: Color(color),
            }));
            drawing.push(CanvasItem::Text(CanvasText {
                bounds: CanvasRect {
                    x: pad.x + 2,
                    y: pad.y + pad.height - 32,
                    width: pad.width - 4,
                    height: 24,
                    color: Color(0xffffffff),
                },
                font_size: if self.keyboard.width < 320 { 12 } else { 14 },
                bold: false,
                text: name,
            }));
        }
        let frames = if self.keyboard.lit.iter().any(|&remaining| remaining > 0)
            || (self.audio.is_some() && !self.ready() && self.error.is_none())
        {
            CanvasFrames::Continuous
        } else {
            CanvasFrames::Static
        };
        view.canvas(CANVAS, self.keyboard.width, HEIGHT, frames, &drawing);
        view.text(
            TextStyle::Muted,
            "↑ ↓ change volume. Notes ring for half a second; playing a ninth replaces the oldest.",
        );
        view.column_end();
        view.submit(context)
    }
}

#[allow(unsafe_code, reason = "The SDK resolves guest registration by its ABI name.")]
#[unsafe(no_mangle)]
pub fn playbit_app() -> playbit_api::App {
    playbit_api::app(Piano::default(), turn, true)
}

fn turn(piano: &mut Piano, context: &mut Context<'_>, view: &mut ViewEncoder<'_>) {
    let mut prepare = false;
    for event in context.raw_events() {
        let result = match event.kind {
            EventKind::Started => piano.open(context),
            EventKind::DataReady => event
                .operation()
                .and_then(|operation| piano.complete(context, operation))
                .map(|ready| prepare |= ready),
            EventKind::SurfaceChanged => Surface::decode(event.data).map(|surface| {
                piano.keyboard.width = surface.width.clamp(240, 960);
                if surface.width == 0 || surface.height == 0 {
                    piano.keyboard.cancel();
                    piano.keyboard.lit.fill(0);
                }
            }),
            EventKind::Click => match event.target {
                QUIETER => piano.change_volume(context, -10),
                LOUDER => piano.change_volume(context, 10),
                RETRY => piano.retry(context),
                _ => Ok(()),
            },
            EventKind::WindowPointer if event.target == CANVAS => {
                event.input().and_then(|(_, input)| {
                    let Input::Pointer(pointer) = input else { return Err(Error::InvalidArgument) };
                    let action = piano.keyboard.pointer(pointer);
                    piano.act(context, action)
                })
            }
            EventKind::WindowKeyboard => event.input().and_then(|(_, input)| {
                let Input::Keyboard(key) = input else { return Err(Error::InvalidArgument) };
                let action = piano.keyboard.key(key.device_key, key.pressed);
                piano.act(context, action)
            }),
            EventKind::Frame if event.target == CANVAS => {
                FrameInput::decode(event.data).map(|frame| {
                    piano.keyboard.advance(frame.elapsed_us.div_ceil(1000));
                    prepare = true;
                })
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            piano.error = Some(format!("{error:?}. Try again."));
        }
    }
    // Prepare at most one note in a turn, driven by device readiness or display
    // frames. The PCM bank is retained for retries and is never resynthesized.
    if prepare
        && piano.error.is_none()
        && let Err(error) = piano.prepare_note(context)
    {
        piano.error = Some(format!("{error:?}. Retry audio to prepare the notes."));
    }
    piano.render(context, view).expect("submit piano controls");
}
