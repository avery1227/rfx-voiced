//! The vocoder chain - where speech stops sounding like a game and starts
//! sounding like a radio.
//!
//! ```text
//! Opus 48k in -> decode -> LPF + decimate x6 -> 8 kHz PCM
//!             -> 300 Hz high-pass + AGC        (once per talker)
//!             -> Codec2 encode                 (once per talker)
//!             -> per-bucket frame errors
//!             -> Codec2 decode                 (once per bucket)
//!             -> raw 8 kHz PCM out             (once per bucket)
//! ```
//!
//! Codec2 is open, patent-free and MBE-adjacent - the same family as the
//! AMBE+2 vocoder P25 Phase 2 actually uses. It is not AMBE and will not be
//! bit-identical, but it fails in the same family of ways, which is the part
//! anyone can hear. See `codec2_mode()` for why the default is 3200 rather
//! than the rate-matched 2400.
//!
//! Everyone on a talkgroup hears the same speech, degraded by THEIR OWN link
//! quality - a 0..100 scale where 100 is full quieting and 0 is below the
//! decode threshold. Running a separate chain per listener would not scale, so
//! listeners are grouped to the nearest `LANE_STEP` and each group shares one
//! Codec2 decoder. Forty listeners cost a handful of decodes, not forty.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use anyhow::Result;
use codec2::{Codec2, Codec2Mode};

/// Radio audio is 8 kHz, full stop.
pub const RATE: u32 = 8000;
/// 160 samples, 20 ms - the same at both Codec2 rates we use.
pub const FRAME: usize = 160;

/// Which Codec2 rate stands in for AMBE+2 half-rate.
///
/// 2400 bps is the rate-matched choice (AMBE+2 half-rate carries 2450 bps of
/// voice). It is also audibly rougher than the real thing, because Codec2 at
/// 2400 is simply not as good a codec as AMBE+2 at 2450 - matching the bitrate
/// matches the wrong property.
///
/// 3200 bps is the closer PERCEPTUAL match, and it is the default. A P25 base
/// station or mobile on a strong signal is clean; if bucket 0 sounds chewed
/// up, the emulation is wrong however defensible the bitrate is.
///
/// Set VOICED_CODEC2_MODE=2400 to hear the difference.
fn codec2_mode() -> Codec2Mode {
    match std::env::var("VOICED_CODEC2_MODE").as_deref() {
        Ok("2400") => Codec2Mode::MODE_2400,
        _ => Codec2Mode::MODE_3200,
    }
}

// ---------------------------------------------------------------------------
// Front-end conditioning
//
// Both of these were in the design and neither was implemented, which is most
// of why a clean bucket sounded rough. A low-rate vocoder is far more
// sensitive to its input than a waveform codec: give it rumble to model, or a
// signal 20 dB below what it expects, and it spends its parameters badly.
// ---------------------------------------------------------------------------

/// RBJ biquad, used here as a 2nd-order high-pass.
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn highpass(fs: f32, f0: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q);

        let b0 = (1.0 + cos) / 2.0;
        let b1 = -(1.0 + cos);
        let b2 = (1.0 + cos) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn lowpass(fs: f32, f0: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q);

        let b0 = (1.0 - cos) / 2.0;
        let b1 = 1.0 - cos;
        let b2 = (1.0 - cos) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Peaking EQ. Used for the presence lift that makes a small
    /// communications speaker intelligible at volume.
    fn peaking(fs: f32, f0: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha / a;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// What the vocoder output goes through on its way out - the speaker, not the
/// codec.
///
/// Raw Codec2 output sounds like a codec. A radio sounds like a small, loud,
/// band-limited speaker being driven slightly too hard, and that character is
/// most of what people recognise. Three cheap stages:
///
///   * band-limit 300-3000 Hz, which is the passband of an actual speaker mic
///   * a presence lift around 1.8 kHz, which is what communications gear does
///     to keep consonants intelligible over engine noise
///   * gentle saturation, because that speaker is always being overdriven
///
/// Skip these and you are listening to a vocoder. Apply them and you are
/// listening to a radio.
struct Speaker {
    hp: Biquad,
    presence: Biquad,
    lp: Biquad,
    drive: f32,
    level: f32,
}

impl Speaker {
    fn new() -> Self {
        Self {
            hp: Biquad::highpass(RATE as f32, 300.0, 0.707),
            presence: Biquad::peaking(RATE as f32, 1800.0, 1.0, 5.0),
            lp: Biquad::lowpass(RATE as f32, 3000.0, 0.707),
            drive: std::env::var("VOICED_SPEAKER_DRIVE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.6),
            // Output trim, set ONCE and here. Everything upstream - the AGC,
            // the presence lift, the saturator's normalisation - drives toward
            // full scale, so without this the sum of three reasonable stages
            // is a radio that shouts.
            //
            // 1.0 was too loud, 0.40 too quiet; this is the midpoint, about
            // -3.7 dBFS. It is the one dial to move for volume - reach for
            // VOICED_SPEAKER_DRIVE only if the problem is harshness rather
            // than level.
            level: std::env::var("VOICED_OUTPUT_LEVEL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.65),
        }
    }

    fn run(&mut self, pcm: &mut [i16]) {
        for s in pcm.iter_mut() {
            let mut x = *s as f32 / 32768.0;

            x = self.hp.run(x);
            x = self.presence.run(x);
            x = self.lp.run(x);

            // Soft clip. tanh keeps the peaks under control while adding the
            // harmonics a driven speaker adds anyway - far kinder than the
            // hard clamp that would otherwise catch the presence lift.
            x = (x * self.drive).tanh() / self.drive.tanh();
            x *= self.level;

            *s = (x * 32767.0).clamp(-32768.0, 32767.0) as i16;
        }
    }
}

/// Slow AGC with a hard limiter, which is what a real radio's front end does
/// and why radio traffic is so consistently loud regardless of how far the
/// operator is holding the mic from their face.
struct Agc {
    gain: f32,
    envelope: f32,
    /// 0..1 gate multiplier, ramped rather than switched.
    gate: f32,
    open: bool,
}

impl Agc {
    /// Target RMS. Deliberately modest: the job is to make a cheap laptop mic
    /// and a good desk mic land in the same place, not to make everything
    /// loud. Level is set once at the output (see `Speaker::level`).
    const TARGET: f32 = 2000.0;
    const MIN_GAIN: f32 = 0.5;
    /// Capped well below what a very quiet mic would "need". Past about 6x you
    /// are no longer lifting a quiet talker, you are lifting their room tone,
    /// their fan, and their keyboard.
    const MAX_GAIN: f32 = 6.0;

    /// Gate. Below `GATE_CLOSE` the input is a noise floor rather than speech
    /// and is pulled down; above `GATE_OPEN` it is speech. The gap between
    /// them is hysteresis, so a trailing consonant does not chatter the gate.
    const GATE_OPEN: f32 = 220.0;
    const GATE_CLOSE: f32 = 110.0;

    fn new() -> Self {
        Self {
            gain: 1.0,
            envelope: 0.0,
            gate: 0.0,
            open: false,
        }
    }

    fn run(&mut self, frame: &mut [f32]) {
        let sum: f32 = frame.iter().map(|s| s * s).sum();
        let rms = (sum / frame.len().max(1) as f32).sqrt();

        // Fast attack, slow release: catches a shout without pumping between
        // words.
        let a = if rms > self.envelope { 0.5 } else { 0.05 };
        self.envelope += a * (rms - self.envelope);

        if self.open {
            if self.envelope < Self::GATE_CLOSE {
                self.open = false;
            }
        } else if self.envelope > Self::GATE_OPEN {
            self.open = true;
        }

        // Ramp rather than switch. A hard gate on a noisy mic is more
        // distracting than the noise it removes.
        let want_gate = if self.open { 1.0 } else { 0.0 };
        let g = if want_gate > self.gate { 0.35 } else { 0.05 };
        self.gate += g * (want_gate - self.gate);

        // Only chase the level while there is speech to chase.
        if self.open {
            let want =
                (Self::TARGET / self.envelope.max(1.0)).clamp(Self::MIN_GAIN, Self::MAX_GAIN);
            self.gain += 0.1 * (want - self.gain);
        }

        for s in frame.iter_mut() {
            *s = (*s * self.gain * self.gate).clamp(-32000.0, 32000.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-bucket error model
//
// P25 does not degrade gracefully. It is excellent, then briefly awful, then
// gone, across about 3 dB. Bucket 5 is SILENCE - a receiver below threshold
// produces nothing at all. Adding hiss there is the single most common mistake
// in game radio and it destroys the illusion immediately.
// ---------------------------------------------------------------------------

/// How audio SOUNDS coming out of a route.
///
/// The rendering belongs to the destination, not to the speaker. A patch
/// cross-connects a P25 talkgroup to a VHF channel, and those are not the same
/// radio system: the P25 side hears a 2400 bps vocoder that freezes when it
/// loses frames, and the VHF side hears an analogue carrier that hisses. One
/// transmission has to be rendered both ways at once.
///
/// Before this existed everything went through the vocoder, so a VHF set
/// patched to P25 heard P25 artifacts - which is backwards, and is the one
/// thing that would give away that the VHF side is not really analogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Mode {
    /// Trunked digital. Codec2 standing in for AMBE+2, with bit errors and
    /// frame erasures.
    #[default]
    P25,
    /// Analogue FM. No vocoder at all - the audio is band-limited and noise is
    /// added, and it degrades smoothly rather than falling off a cliff.
    Fm,
    /// Analogue AM, as on airband. Noisier for the same link, because AM has
    /// no limiter to throw amplitude noise away.
    Am,
}

#[derive(Debug, Clone, Copy)]
pub struct ErrorProfile {
    /// Probability a whole frame is lost; the decoder repeats the previous one.
    pub erasure: f32,
    /// Bit errors applied to a surviving frame.
    pub bit_errors: u32,
    /// Nothing is delivered at all.
    pub muted: bool,
}

/// Anchor points on the quality curve: (quality, erasure rate, bit errors).
///
/// Quality runs 0..100, where 100 is a full-quieting signal and 0 is below
/// the decode threshold. Values between anchors are interpolated, so the
/// caller gets continuous control rather than six steps.
///
/// The spacing is deliberately non-linear and bunched at the low end, because
/// P25 is not linear: it is excellent, then briefly awful, then gone, across
/// about 3 dB. Most of the interesting behaviour lives between 40 and 10.
const CURVE: &[(f32, f32, f32)] = &[
    (100.0, 0.000, 0.0),
    (90.0, 0.005, 1.0),
    (75.0, 0.030, 2.0),
    (55.0, 0.120, 3.0),
    (30.0, 0.350, 6.0),
    (10.0, 0.600, 8.0),
];

/// Below this nothing decodes at all. SILENCE, never static - a receiver under
/// threshold produces nothing, and adding hiss here is the single most common
/// mistake in game radio.
pub const DECODE_FLOOR: u8 = 5;

/// Lane granularity. Listeners are grouped to the nearest step so that a
/// talkgroup needs a handful of decoder chains rather than one per listener.
/// A lane is only a Codec2 decode now that lanes emit raw PCM, so this can be
/// far finer than the six buckets it replaces.
pub const LANE_STEP: u8 = 5;

pub fn lane_of(quality: u8) -> u8 {
    let q = quality.min(100);
    if q <= DECODE_FLOOR {
        return 0;
    }
    ((q + LANE_STEP / 2) / LANE_STEP) * LANE_STEP
}

pub fn profile(quality: u8) -> ErrorProfile {
    let q = quality.min(100) as f32;

    if quality <= DECODE_FLOOR {
        return ErrorProfile {
            erasure: 1.0,
            bit_errors: 0,
            muted: true,
        };
    }
    if q >= CURVE[0].0 {
        return ErrorProfile {
            erasure: CURVE[0].1,
            bit_errors: 0,
            muted: false,
        };
    }

    for w in CURVE.windows(2) {
        let (hi_q, hi_e, hi_b) = w[0];
        let (lo_q, lo_e, lo_b) = w[1];
        if q <= hi_q && q >= lo_q {
            let t = (hi_q - q) / (hi_q - lo_q);
            return ErrorProfile {
                erasure: hi_e + t * (lo_e - hi_e),
                bit_errors: (hi_b + t * (lo_b - hi_b)).round() as u32,
                muted: false,
            };
        }
    }

    // Below the last anchor but above the floor: worst decodable signal.
    let last = CURVE[CURVE.len() - 1];
    ErrorProfile {
        erasure: last.1,
        bit_errors: last.2 as u32,
        muted: false,
    }
}

/// xorshift64*, so the error model needs no `rand` dependency and is
/// reproducible when seeded - which matters for testing artifacts by ear.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn chance(&mut self, p: f32) -> bool {
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p as f64
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next() % n as u64) as u32
    }
}

// ---------------------------------------------------------------------------
// 48 kHz -> 8 kHz
//
// Naive decimation by 6 aliases badly, and aliasing sounds broken rather than
// lo-fi - the opposite of what we want. A windowed-sinc low-pass at 3.4 kHz
// first costs 48 multiplies per output sample, which at 8000 outputs a second
// is nothing.
// ---------------------------------------------------------------------------

pub struct Decimator {
    taps: Vec<f32>,
    history: Vec<f32>,
    phase: usize,
}

impl Decimator {
    pub fn new() -> Self {
        const N: usize = 48;
        let fc = 3400.0 / 48000.0; // normalised cutoff
        let mid = (N - 1) as f32 / 2.0;

        let mut taps = Vec::with_capacity(N);
        for n in 0..N {
            let x = n as f32 - mid;
            let sinc = if x.abs() < 1e-6 {
                2.0 * fc
            } else {
                (2.0 * std::f32::consts::PI * fc * x).sin() / (std::f32::consts::PI * x)
            };
            // Hamming window
            let w = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * n as f32 / (N - 1) as f32).cos();
            taps.push(sinc * w);
        }

        let sum: f32 = taps.iter().sum();
        for t in taps.iter_mut() {
            *t /= sum;
        }

        Self {
            taps,
            history: vec![0.0; N],
            phase: 0,
        }
    }

    /// Feeds 48 kHz samples, emits 8 kHz samples (one per six in).
    pub fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        for &s in input {
            self.history.rotate_left(1);
            *self.history.last_mut().unwrap() = s as f32;

            self.phase += 1;
            if self.phase == 6 {
                self.phase = 0;
                let mut acc = 0.0;
                for (h, t) in self.history.iter().zip(self.taps.iter()) {
                    acc += h * t;
                }
                out.push(acc.clamp(-32768.0, 32767.0) as i16);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Lanes
// ---------------------------------------------------------------------------

/// One listener's link, and the rendering it should get.
///
/// `bridged` is the thing that makes a patch sound like a patch. A patch is a
/// physical bridge: the audio is DEMODULATED on one side and RE-MODULATED onto
/// the other, so a listener across one has been through two RF hops, not one,
/// and the artifacts of both are present. That is why patched audio is
/// notoriously worse than either system on its own, and why anybody who has
/// heard a real one recognises it instantly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sink {
    pub mode: Mode,
    /// 0..100, where 100 is full quieting.
    pub quality: u8,
    /// This listener is on the far side of a patch from the speaker.
    pub bridged: bool,
}

/// One RF hop: what a transmission sounds like after crossing it.
///
/// Separate state per hop is required rather than an optimisation. A corrupted
/// vocoder frame changes decoder state and that divergence IS the artifact, and
/// an analogue filter carries history the same way - so a degraded lane must
/// never inherit a clean lane's.
///
/// Hops emit raw 8 kHz PCM rather than re-encoded Opus. CEF cannot be relied on
/// to decode bare Opus packets, and at 8 kHz mono the wire cost is 128 kbps per
/// talker - cheap enough that adding a codec back would be buying risk with
/// bandwidth we are not short of.
enum Leg {
    // Boxed: the vocoder state is two Codec2 instances and dwarfs the
    // analogue variant, so an unboxed enum would make every analogue hop pay
    // fifteen kilobytes for a struct it does not have.
    Digital(Box<DigitalLeg>),
    Analog(AnalogLeg),
}

impl Leg {
    /// `output` is false for the middle of a bridge. A patch is a wire, not a
    /// loudspeaker: running the speaker chain there too would apply its
    /// saturation and trim twice, which sounds like a bad recording rather
    /// than like a second radio.
    fn new(sink: Sink, output: bool, frame_bytes: usize, seed: u64) -> Self {
        let speaker = output.then(Speaker::new);
        match sink.mode {
            Mode::P25 => Leg::Digital(Box::new(DigitalLeg {
                encoder: Codec2::new(codec2_mode()),
                decoder: Codec2::new(codec2_mode()),
                previous: vec![0u8; frame_bytes],
                packed: vec![0u8; frame_bytes],
                rng: Rng(0x9E37_79B9_7F4A_7C15 ^ seed),
                profile: profile(lane_of(sink.quality)),
                frame_bytes,
                speaker,
            })),
            Mode::Fm | Mode::Am => Leg::Analog(AnalogLeg {
                rng: Rng(0x8EBC_6AF0_9C88_C6E3 ^ seed),
                tilt: Biquad::lowpass(RATE as f32, 3400.0, 0.707),
                mode: sink.mode,
                noise: analog_noise(sink.quality, sink.mode),
                speaker,
            }),
        }
    }

    /// Crosses one hop. `frame` is exactly one vocoder frame; the same number
    /// of samples comes out.
    fn run(&mut self, frame: &[i16], out: &mut Vec<i16>) {
        match self {
            Leg::Digital(d) => d.run(frame, out),
            Leg::Analog(a) => a.run(frame, out),
        }
    }
}

/// A digital hop: vocode, corrupt, decode.
struct DigitalLeg {
    encoder: Codec2,
    decoder: Codec2,
    previous: Vec<u8>,
    packed: Vec<u8>,
    rng: Rng,
    profile: ErrorProfile,
    frame_bytes: usize,
    speaker: Option<Speaker>,
}

impl DigitalLeg {
    fn run(&mut self, frame: &[i16], out: &mut Vec<i16>) {
        self.encoder.encode(&mut self.packed, frame);

        let mut corrupted = self.packed.clone();
        if self.rng.chance(self.profile.erasure) {
            // Frame erased: the decoder repeats the previous one. This is
            // "the freeze", and it is a real P25 artifact.
            corrupted.copy_from_slice(&self.previous);
        } else {
            for _ in 0..self.profile.bit_errors {
                let bit = self.rng.below((self.frame_bytes * 8) as u32) as usize;
                corrupted[bit / 8] ^= 1 << (bit % 8);
            }
            self.previous.copy_from_slice(&self.packed);
        }

        let mut pcm = vec![0i16; frame.len()];
        self.decoder.decode(&mut pcm, &corrupted);
        if let Some(sp) = self.speaker.as_mut() {
            sp.run(&mut pcm);
        }
        out.extend_from_slice(&pcm);
    }
}

/// An analogue hop: band-limit, and bury it in as much noise as the link
/// deserves.
///
/// Nothing here is a vocoder. Analogue radio carries the waveform, so what the
/// far end produces is what was spoken, filtered by the channel and mixed with
/// noise. The two things that make it recognisably analogue rather than
/// "digital with hiss":
///
///   * Noise scales smoothly with signal. No decode threshold, no freeze, no
///     cliff - a marginal FM signal is scratchy and still usable, which is the
///     entire reason a fireground keeps conventional around.
///   * FM limits and AM does not. An FM receiver throws amplitude away, so its
///     noise floor is flat and sits under the voice; AM passes amplitude
///     straight through, so the noise rides on the signal and is worst exactly
///     when somebody is talking.
struct AnalogLeg {
    rng: Rng,
    /// Lowpass on the noise itself, so it is a hiss rather than a fizz.
    tilt: Biquad,
    mode: Mode,
    noise: f32,
    speaker: Option<Speaker>,
}

impl AnalogLeg {
    fn run(&mut self, frame: &[i16], out: &mut Vec<i16>) {
        let mut pcm: Vec<i16> = Vec::with_capacity(frame.len());

        for s in frame {
            let clean = *s as f32 / 32768.0;

            // Uniform, then filtered. Good enough at this level, and cheaper
            // than a gaussian nobody could pick out by ear.
            let raw = ((self.rng.next() >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0;
            let hiss = self.tilt.run(raw) * self.noise;

            let x = match self.mode {
                Mode::Fm => clean + hiss,
                Mode::Am => clean + hiss * (0.6 + clean.abs() * 1.4),
                Mode::P25 => clean,
            };

            pcm.push((x.clamp(-1.0, 1.0) * 32767.0) as i16);
        }

        if let Some(sp) = self.speaker.as_mut() {
            sp.run(&mut pcm);
        }
        out.extend_from_slice(&pcm);
    }
}

/// Noise amplitude for a link, 0..1.
///
/// Deliberately smooth and deliberately never zero. Full quieting on a real FM
/// receiver is quiet, not silent, and that residual is most of what tells an
/// operator the channel is open rather than dead.
fn analog_noise(quality: u8, mode: Mode) -> f32 {
    let q = quality.min(100) as f32 / 100.0;

    // Squared, so the top of the range is nearly clean and the bottom falls
    // apart quickly, which is the shape of the real curve.
    let base = (1.0 - q).powi(2) * 0.55 + 0.004;

    match mode {
        // Worse for the same link: no limiter to throw amplitude noise away.
        Mode::Am => base * 1.6,
        _ => base,
    }
}

/// A stable key for one rendering. Two listeners sharing it hear byte-identical
/// audio and are served from one pass.
pub type LegKey = (Mode, u8, bool);

pub fn key_of(sink: Sink) -> LegKey {
    (sink.mode, lane_of(sink.quality), sink.bridged)
}

/// One talker's chain: shared front end, one hop per rendering in use.
pub struct Talker {
    opus_in: opus::Decoder,
    decimator: Decimator,
    highpass: Biquad,
    agc: Agc,
    /// 8 kHz samples not yet consumed by a whole vocoder frame.
    pending: Vec<i16>,
    /// The SPEAKER's own hop, rendered once per frame and shared by every
    /// listener on the far side of a patch. This is the demodulated audio a
    /// patch device would be feeding into the other system.
    bridge: Option<(Sink, Leg)>,
    /// Destination hops, one per rendering actually in use.
    legs: HashMap<LegKey, Leg>,
    spf: usize,
    frame_bytes: usize,
}

impl Talker {
    pub fn new() -> Result<Self> {
        let probe = Codec2::new(codec2_mode());
        Ok(Self {
            // FiveM encodes at 48 kHz mono.
            opus_in: opus::Decoder::new(48000, opus::Channels::Mono)?,
            decimator: Decimator::new(),
            // 300 Hz: below the voice band, and rumble a vocoder would
            // otherwise waste parameters modelling.
            highpass: Biquad::highpass(RATE as f32, 300.0, 0.707),
            agc: Agc::new(),
            pending: Vec::with_capacity(FRAME * 4),
            bridge: None,
            legs: HashMap::new(),
            spf: probe.samples_per_frame(),
            frame_bytes: probe.bits_per_frame().div_ceil(8),
        })
    }

    /// Feeds one Opus packet from the tap and returns the PCM for each
    /// rendering in use.
    ///
    /// `src` is the SPEAKER's own link and the mode of the route they keyed;
    /// `sinks` is one entry per listener. Listeners are grouped internally, so
    /// passing forty is fine.
    ///
    /// A P25 listener below the decode threshold produces nothing at all - not
    /// silence samples, nothing - because that is what a receiver below
    /// threshold does. An analogue listener always produces something, because
    /// analogue has no threshold: a bad FM signal is noise, not absence, and
    /// that difference is most of what separates the two systems.
    pub fn push(
        &mut self,
        opus_packet: &[u8],
        src: Sink,
        sinks: &[Sink],
    ) -> Result<Vec<(LegKey, Vec<i16>)>> {
        // 48 kHz mono, worst case 60 ms.
        let mut wide = vec![0i16; 48000 * 60 / 1000];
        let n = self.opus_in.decode(opus_packet, &mut wide, false)?;
        wide.truncate(n);

        self.push_wide(&wide, src, sinks)
    }

    /// The same chain, fed 48 kHz PCM directly.
    ///
    /// A dispatch console has no Opus in the path: the browser captures at the
    /// device rate and sends samples, so decoding would mean encoding first,
    /// purely to decode it again. Everything downstream is the same code, so a
    /// console and a radio sound like the same network rather than like two
    /// systems.
    pub fn push_pcm(
        &mut self,
        wide: &[i16],
        src: Sink,
        sinks: &[Sink],
    ) -> Result<Vec<(LegKey, Vec<i16>)>> {
        self.push_wide(wide, src, sinks)
    }

    fn push_wide(
        &mut self,
        wide: &[i16],
        src: Sink,
        sinks: &[Sink],
    ) -> Result<Vec<(LegKey, Vec<i16>)>> {
        self.decimator.push(wide, &mut self.pending);

        let mut out: Vec<(LegKey, Vec<i16>)> = Vec::new();
        for s in sinks {
            // Only digital has a threshold below which nothing arrives.
            if s.mode == Mode::P25 && profile(lane_of(s.quality)).muted {
                continue;
            }
            let key = key_of(*s);
            if !out.iter().any(|(k, _)| *k == key) {
                out.push((key, Vec::new()));
            }
        }

        if out.is_empty() {
            // Nobody to serve; still drain so state does not grow unbounded.
            while self.pending.len() >= self.spf {
                self.pending.drain(..self.spf);
            }
            return Ok(out);
        }

        // The bridge is built only when somebody is actually across one, and
        // rebuilt when the speaker's own link changes bucket - its error state
        // belongs to one link, not to a talkspurt.
        let need_bridge = out.iter().any(|((_, _, bridged), _)| *bridged);
        if need_bridge {
            let stale = match &self.bridge {
                Some((have, _)) => key_of(*have) != key_of(src),
                None => true,
            };
            if stale {
                self.bridge = Some((src, Leg::new(src, false, self.frame_bytes, seed_of(src, 1))));
            }
        }

        let mut work = vec![0f32; self.spf];
        let mut bridged_frame: Vec<i16> = Vec::with_capacity(self.spf);

        while self.pending.len() >= self.spf {
            let raw: Vec<i16> = self.pending.drain(..self.spf).collect();

            // Condition BEFORE anything else. A low-rate vocoder resynthesises
            // from parameters, so rumble or an inconsistent level costs quality
            // across the whole frame - and an analogue hop would carry them
            // through untouched.
            for (w, r) in work.iter_mut().zip(raw.iter()) {
                *w = self.highpass.run(*r as f32);
            }
            self.agc.run(&mut work);
            let clean: Vec<i16> = work.iter().map(|s| *s as i16).collect();

            // The speaker's own hop, once. This is what a patch device hears
            // and re-modulates: a VHF unit's audio arrives at the patch with
            // its hiss already on it, and that hiss then goes through the P25
            // vocoder - which is exactly why patched audio sounds worse than
            // either side alone.
            bridged_frame.clear();
            if need_bridge {
                if let Some((_, leg)) = self.bridge.as_mut() {
                    leg.run(&clean, &mut bridged_frame);
                }
            }

            for (key, pcm_out) in out.iter_mut() {
                let (mode, lane, bridged) = *key;
                let input = if bridged { &bridged_frame } else { &clean };

                let leg = self.legs.entry(*key).or_insert_with(|| {
                    Leg::new(
                        Sink {
                            mode,
                            quality: lane,
                            bridged,
                        },
                        true,
                        self.frame_bytes,
                        seed_of(
                            Sink {
                                mode,
                                quality: lane,
                                bridged,
                            },
                            2,
                        ),
                    )
                });
                leg.run(input, pcm_out);
            }
        }

        Ok(out)
    }
}

/// Seeds the noise and error streams. Per rendering, so two listeners on the
/// same link do not hear bit-identical noise - which reads as a recording
/// rather than as radio - and stable, so one listener's hiss does not restart
/// every frame.
fn seed_of(sink: Sink, salt: u64) -> u64 {
    (sink.quality as u64).wrapping_mul(0x1234_5678)
        ^ ((sink.mode as u64) << 40)
        ^ ((sink.bridged as u64) << 48)
        ^ salt.wrapping_mul(0x9E37_79B9)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_48k(ms: usize) -> Vec<i16> {
        let n = 48 * ms;
        (0..n)
            .map(|i| {
                let t = i as f32 / 48000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 8000.0) as i16
            })
            .collect()
    }

    #[test]
    fn the_quality_curve_is_monotonic_and_bounded() {
        assert!(profile(100).erasure == 0.0 && !profile(100).muted);
        assert!(profile(0).muted, "0 is below the decode floor");
        assert!(profile(DECODE_FLOOR).muted);

        let mut last = -1.0f32;
        for q in (DECODE_FLOOR + 1)..=100 {
            let e = profile(q).erasure;
            assert!((0.0..=1.0).contains(&e), "erasure out of range at {q}");
            if last >= 0.0 {
                assert!(
                    e <= last + 1e-6,
                    "quality {q} must not be worse than {}",
                    q - 1
                );
            }
            last = e;
        }
    }

    #[test]
    fn lanes_group_listeners() {
        assert_eq!(lane_of(100), 100);
        assert_eq!(lane_of(98), 100, "nearby listeners share a lane");
        assert_eq!(lane_of(97), 95);
        assert_eq!(lane_of(3), 0, "below the floor is one silent lane");
    }

    #[test]
    fn codec2_2400_frames_are_160_samples_and_48_bits() {
        let c2 = Codec2::new(Codec2Mode::MODE_2400);
        assert_eq!(c2.samples_per_frame(), FRAME, "20 ms at 8 kHz");
        assert_eq!(c2.bits_per_frame(), 48, "2400 bps");
    }

    #[test]
    fn decimator_produces_one_sample_per_six() {
        let mut d = Decimator::new();
        let mut out = Vec::new();
        d.push(&tone_48k(40), &mut out);
        assert_eq!(out.len(), 40 * 8, "40 ms at 8 kHz");
    }

    #[test]
    fn decimator_rejects_content_above_the_passband() {
        // 12 kHz would alias to 4 kHz without the low-pass, which sounds
        // broken rather than lo-fi.
        let n = 48 * 200;
        let alias: Vec<i16> = (0..n)
            .map(|i| {
                let t = i as f32 / 48000.0;
                ((t * 12000.0 * 2.0 * std::f32::consts::PI).sin() * 16000.0) as i16
            })
            .collect();

        let mut d = Decimator::new();
        let mut out = Vec::new();
        d.push(&alias, &mut out);

        let settled = &out[out.len() / 2..];
        let peak = settled
            .iter()
            .map(|s| s.unsigned_abs() as u32)
            .max()
            .unwrap_or(0);
        assert!(
            peak < 1600,
            "out-of-band tone should be attenuated, peak was {peak}"
        );
    }

    /// A P25 subscriber on their own talkgroup: one hop, no bridge.
    fn p25(quality: u8) -> Sink {
        Sink {
            mode: Mode::P25,
            quality,
            bridged: false,
        }
    }

    fn opus_40ms(enc: &mut opus::Encoder) -> Result<Vec<u8>> {
        let pcm = tone_48k(40);
        let mut packet = vec![0u8; 4000];
        let len = enc.encode(&pcm, &mut packet)?;
        packet.truncate(len);
        Ok(packet)
    }

    #[test]
    fn full_quieting_produces_audio_and_below_threshold_produces_none() -> Result<()> {
        let mut t = Talker::new()?;
        let mut encoder = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)?;
        let packet = opus_40ms(&mut encoder)?;

        let out = t.push(&packet, p25(100), &[p25(100), p25(0)])?;

        let clean = out
            .iter()
            .find(|((_, lane, _), _)| *lane == 100)
            .expect("quality 100 is served");
        assert_eq!(
            clean.1.len(),
            2 * FRAME,
            "40 ms yields two 20 ms vocoder frames"
        );
        assert!(clean.1.iter().any(|s| *s != 0), "and it is not silence");

        assert!(
            out.iter().all(|((_, lane, _), _)| *lane != 0),
            "below the decode floor delivers nothing at all - silence, not noise"
        );
        Ok(())
    }

    #[test]
    fn a_degraded_lane_diverges_from_the_clean_one() -> Result<()> {
        let mut t = Talker::new()?;
        let mut encoder = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)?;
        let mut differed = false;

        for _ in 0..25 {
            let packet = opus_40ms(&mut encoder)?;
            let out = t.push(&packet, p25(100), &[p25(100), p25(30)])?;
            let clean = &out.iter().find(|((_, l, _), _)| *l == 100).unwrap().1;
            let rough = &out.iter().find(|((_, l, _), _)| *l == 30).unwrap().1;
            if clean != rough {
                differed = true;
                break;
            }
        }

        assert!(
            differed,
            "quality 30 must not sound identical to quality 100"
        );
        Ok(())
    }

    // -- Patching between systems ------------------------------------------

    #[test]
    fn analogue_has_no_decode_threshold() -> Result<()> {
        let mut t = Talker::new()?;
        let mut encoder = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)?;
        let packet = opus_40ms(&mut encoder)?;

        let fm = Sink {
            mode: Mode::Fm,
            quality: 0,
            bridged: false,
        };
        let out = t.push(&packet, fm, &[fm])?;

        // The P25 side of the same link gets nothing at all. FM gets noise,
        // because that is the difference between the two systems and the whole
        // reason a fireground keeps conventional around.
        assert_eq!(out.len(), 1);
        assert!(out[0].1.iter().any(|s| *s != 0));
        Ok(())
    }

    #[test]
    fn a_patched_listener_is_rendered_for_their_own_system() -> Result<()> {
        let mut t = Talker::new()?;
        let mut encoder = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)?;
        let packet = opus_40ms(&mut encoder)?;

        // A VHF unit talking, heard by another VHF set on the same channel and
        // by a P25 subscriber across a patch.
        let src = Sink {
            mode: Mode::Fm,
            quality: 80,
            bridged: false,
        };
        let out = t.push(
            &packet,
            src,
            &[
                src,
                Sink {
                    mode: Mode::P25,
                    quality: 80,
                    bridged: true,
                },
            ],
        )?;

        assert_eq!(out.len(), 2, "two systems, two renderings");
        let direct = out.iter().find(|((m, _, _), _)| *m == Mode::Fm).unwrap();
        let patched = out.iter().find(|((m, _, _), _)| *m == Mode::P25).unwrap();

        assert!(patched.0 .2, "the P25 side crossed a bridge");
        assert_ne!(
            direct.1, patched.1,
            "the same audio must not be delivered to both sides unchanged"
        );
        assert_eq!(direct.1.len(), patched.1.len());
        Ok(())
    }

    #[test]
    fn crossing_a_patch_costs_more_than_staying_put() -> Result<()> {
        let mut t = Talker::new()?;
        let mut encoder = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)?;

        // Two P25 subscribers on the same excellent link. One is on the
        // talkgroup that was keyed; the other is across a patch from a VHF
        // channel, so their audio went through an analogue hop before the
        // vocoder ever saw it.
        let src = Sink {
            mode: Mode::Fm,
            quality: 100,
            bridged: false,
        };
        let direct_sink = Sink {
            mode: Mode::Fm,
            quality: 100,
            bridged: false,
        };
        let patched_sink = Sink {
            mode: Mode::P25,
            quality: 100,
            bridged: true,
        };

        let mut differed = false;
        for _ in 0..10 {
            let packet = opus_40ms(&mut encoder)?;
            let out = t.push(&packet, src, &[direct_sink, patched_sink])?;
            let a = &out.iter().find(|((m, _, _), _)| *m == Mode::Fm).unwrap().1;
            let b = &out.iter().find(|((m, _, _), _)| *m == Mode::P25).unwrap().1;
            if a != b {
                differed = true;
                break;
            }
        }

        assert!(
            differed,
            "a patched listener hears the extra hop, not a clean copy"
        );
        Ok(())
    }

    #[test]
    fn am_is_noisier_than_fm_on_the_same_link() {
        for q in [20u8, 60, 90] {
            assert!(
                analog_noise(q, Mode::Am) > analog_noise(q, Mode::Fm),
                "AM has no limiter to throw amplitude noise away (quality {q})"
            );
        }
        assert!(
            analog_noise(100, Mode::Fm) > 0.0,
            "full quieting is quiet, not silent - the residual is how an \
             operator knows the channel is open rather than dead"
        );
    }
}

// -- AM ---------------------------------------------------------------------

/// Samples in one 20 ms frame at 8 kHz. The vocoder's natural unit.
const AM_FRAME: usize = 160;

/// A talker is treated as gone once its queue has been dry this long. Without
/// it, one radio unkeying would stall everybody else waiting for samples that
/// are never coming.
const AM_GONE_MS: u64 = 120;

/// Mixes concurrent transmissions on one AM frequency.
///
/// FM captures and AM does not: on airband both carriers reach the receiver,
/// the envelope detector sums them, and their carriers - never exactly on
/// frequency - beat together into a heterodyne squeal at the difference. The
/// result is two voices and a whistle, none of it intelligible.
///
/// This has to happen HERE rather than at each client. The frames of two
/// talkers arriving at one listener would otherwise interleave into alternating
/// chunks of each, which sounds like neither. Mixing at the node also means
/// every receiver hears the same collision, which is what a shared frequency
/// means.
#[derive(Default)]
pub struct AmMix {
    /// Pending samples per (lane, talker). Lanes are kept apart because a
    /// listener on a bad link and one on a good link are hearing different
    /// renderings of the same collision.
    queues: HashMap<(LegKey, u32), VecDeque<i16>>,
    /// When each talker last contributed anything.
    seen: HashMap<u32, Instant>,
    phase: f32,
}

impl AmMix {
    pub fn add(&mut self, lane: LegKey, talker: u32, pcm: &[i16], now: Instant) {
        self.seen.insert(talker, now);
        self.queues
            .entry((lane, talker))
            .or_default()
            .extend(pcm.iter().copied());
    }

    /// Everything still queued, whether or not every talker has contributed.
    ///
    /// For the moment a collision ends. The frames still held belong to
    /// whoever is left, and discarding them clips their first word after the
    /// other radio let go - which is exactly the word somebody was straining
    /// to hear through the mess.
    pub fn flush(&mut self) -> Vec<(LegKey, Vec<i16>)> {
        let mut lanes: Vec<LegKey> = self.queues.keys().map(|(lane, _)| *lane).collect();
        lanes.sort_unstable();
        lanes.dedup();

        let mut out: Vec<(LegKey, Vec<i16>)> = Vec::new();
        for lane in lanes {
            let talkers: Vec<u32> = self
                .queues
                .keys()
                .filter(|(l, _)| *l == lane)
                .map(|(_, t)| *t)
                .collect();

            let longest = talkers
                .iter()
                .filter_map(|t| self.queues.get(&(lane, *t)).map(|q| q.len()))
                .max()
                .unwrap_or(0);

            let mut pcm: Vec<i16> = Vec::with_capacity(longest);
            for _ in 0..longest {
                let mut sum: i32 = 0;
                for t in &talkers {
                    if let Some(q) = self.queues.get_mut(&(lane, *t)) {
                        sum += i32::from(q.pop_front().unwrap_or(0));
                    }
                }
                // No heterodyne: the collision is over by the time this runs.
                pcm.push(sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            }

            if !pcm.is_empty() {
                out.push((lane, pcm));
            }
        }

        self.queues.clear();
        self.seen.clear();
        out
    }

    /// Whole frames that every present talker has contributed to.
    ///
    /// Self-clocking off whoever is transmitting: a frame comes out as soon as
    /// the last contributor supplies its share, so the mix drifts by at most
    /// one frame behind the slowest radio.
    pub fn drain(&mut self, now: Instant) -> Vec<(LegKey, Vec<i16>)> {
        // Whoever has stopped is no longer waited for.
        self.seen
            .retain(|_, at| now.duration_since(*at).as_millis() as u64 <= AM_GONE_MS);
        self.queues.retain(|(_, t), _| self.seen.contains_key(t));

        let lanes: Vec<LegKey> = {
            let mut v: Vec<LegKey> = self.queues.keys().map(|(lane, _)| *lane).collect();
            v.sort_unstable();
            v.dedup();
            v
        };

        let mut out: Vec<(LegKey, Vec<i16>)> = Vec::new();

        for lane in lanes {
            let talkers: Vec<u32> = self
                .queues
                .keys()
                .filter(|(l, _)| *l == lane)
                .map(|(_, t)| *t)
                .collect();

            let mut pcm: Vec<i16> = Vec::new();
            loop {
                let ready = talkers.iter().all(|t| {
                    self.queues
                        .get(&(lane, *t))
                        .is_some_and(|q| q.len() >= AM_FRAME)
                });
                if !ready {
                    break;
                }

                for _ in 0..AM_FRAME {
                    let mut sum: i32 = 0;
                    for t in &talkers {
                        if let Some(q) = self.queues.get_mut(&(lane, *t)) {
                            sum += i32::from(q.pop_front().unwrap_or(0));
                        }
                    }

                    // The heterodyne. Two carriers a few hundred hertz apart,
                    // which is what makes a collision on AM recognisable as
                    // one rather than as somebody with a bad microphone. Only
                    // present while more than one radio is up.
                    if talkers.len() > 1 {
                        self.phase += std::f32::consts::TAU * AM_BEAT_HZ / RATE as f32;
                        if self.phase > std::f32::consts::TAU {
                            self.phase -= std::f32::consts::TAU;
                        }
                        sum += (self.phase.sin() * 2200.0) as i32;
                    }

                    pcm.push(sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
                }
            }

            if !pcm.is_empty() {
                out.push((lane, pcm));
            }
        }

        out
    }
}

/// The beat note. Real carriers are never exactly co-channel, and a few
/// hundred hertz is both typical and firmly in the range that ruins speech.
const AM_BEAT_HZ: f32 = 620.0;

#[cfg(test)]
mod am_tests {
    use super::*;

    /// One AM rendering. The mixer keys on the whole rendering, not on a bare
    /// lane: an AM listener across a patch and one on the channel itself hear
    /// different things and must not be summed into each other.
    fn am(lane: u8) -> LegKey {
        (Mode::Am, lane, false)
    }

    fn tone(n: usize, v: i16) -> Vec<i16> {
        vec![v; n]
    }

    #[test]
    fn one_talker_alone_is_held_until_the_frame_is_whole() {
        let mut mix = AmMix::default();
        let now = Instant::now();

        mix.add(am(0), 1, &tone(80, 100), now);
        assert!(mix.drain(now).is_empty(), "half a frame is not a frame");

        mix.add(am(0), 1, &tone(80, 100), now);
        let out = mix.drain(now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.len(), AM_FRAME);
    }

    #[test]
    fn two_talkers_are_summed_not_interleaved() {
        let mut mix = AmMix::default();
        let now = Instant::now();

        // The mixer only learns a talker exists when they contribute, so the
        // first frame of a collision goes out on its own. One frame - 20 ms -
        // and it is what actually happens anyway: the second radio keyed a
        // moment after the first.
        mix.add(am(0), 1, &tone(AM_FRAME, 1000), now);
        assert_eq!(mix.drain(now).len(), 1);

        // From here both are known, and neither is emitted alone.
        mix.add(am(0), 2, &tone(AM_FRAME, 2000), now);
        assert!(
            mix.drain(now).is_empty(),
            "the other radio has not supplied its share yet, and emitting this              frame now would be interleaving rather than mixing"
        );

        mix.add(am(0), 1, &tone(AM_FRAME, 1000), now);
        let out = mix.drain(now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.len(), AM_FRAME);

        // 1000 + 2000. Averaged across the frame, because the heterodyne
        // rides on top at a couple of thousand counts either way - which is
        // the point of it - and integrates to nothing over a dozen cycles.
        let mean: i32 = out[0].1.iter().map(|s| i32::from(*s)).sum::<i32>() / AM_FRAME as i32;
        assert!(
            (2600..=3400).contains(&mean),
            "summed, not captured: mean {mean}"
        );

        // And the beat is actually present, or this is only addition.
        let peak = out[0].1.iter().map(|s| i32::from(*s)).max().unwrap();
        assert!(peak > 4000, "no heterodyne: peak {peak}");
    }

    #[test]
    fn lanes_stay_apart() {
        let mut mix = AmMix::default();
        let now = Instant::now();

        for lane in [am(0), am(2)] {
            mix.add(lane, 1, &tone(AM_FRAME, 500), now);
            mix.add(lane, 2, &tone(AM_FRAME, 500), now);
        }

        let out = mix.drain(now);
        assert_eq!(
            out.len(),
            2,
            "a bad link and a good one are different mixes"
        );
        assert_eq!(out[0].0, am(0));
        assert_eq!(out[1].0, am(2));
    }

    #[test]
    fn a_talker_who_stopped_is_not_waited_for_forever() {
        let mut mix = AmMix::default();
        let start = Instant::now();

        mix.add(am(0), 1, &tone(AM_FRAME, 1000), start);
        mix.add(am(0), 2, &tone(AM_FRAME * 2, 1000), start);
        assert_eq!(mix.drain(start).len(), 1);

        // Talker 1 let go. Talker 2 must not be stuck holding a frame waiting
        // for samples that are never coming.
        let later = start + std::time::Duration::from_millis(AM_GONE_MS + 20);
        mix.add(am(0), 2, &tone(AM_FRAME, 1000), later);
        let out = mix.drain(later);
        assert_eq!(out.len(), 1);
        assert!(out[0].1.len() >= AM_FRAME);
    }

    #[test]
    fn flush_does_not_clip_whoever_is_left() {
        let mut mix = AmMix::default();
        let now = Instant::now();

        mix.add(am(0), 1, &tone(90, 1000), now);
        let out = mix.flush();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.len(), 90, "a partial frame still gets heard");
        assert!(mix.flush().is_empty());
    }
}
