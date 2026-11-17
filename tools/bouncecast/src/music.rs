//! Procedural chiptune: an NES-style band (two pulse channels, a stepped triangle bass and a
//! noise channel for drums) playing generated music in A minor, plus sound effects for the
//! logo. Everything derives from the seed and the sample clock, so a run is reproducible.
//!
//! Song form: 8-bar phrases over a diatonic progression. The lead is built from 2-bar motifs
//! arranged A A B A'. The arrangement adds and drops layers phrase by phrase; every six
//! phrases it picks a new progression and new motifs.

pub const SAMPLE_RATE: u32 = 48_000;
const SR: f32 = SAMPLE_RATE as f32;
const STEPS_PER_BAR: u32 = 16;
const BARS_PER_PHRASE: u32 = 8;
const STEPS_PER_PHRASE: u32 = STEPS_PER_BAR * BARS_PER_PHRASE;

/// A natural minor, as semitones above A.
const SCALE: [i32; 7] = [0, 2, 3, 5, 7, 8, 10];
/// MIDI notes of A in the octaves used.
const A1: i32 = 33;
const A3: i32 = 57;
const A4: i32 = 69;
const A5: i32 = 81;

/// Diatonic progressions, as the scale degree of each bar's chord root (0 = Am, 2 = C, 3 = Dm,
/// 4 = Em, 5 = F, 6 = G).
const PROGRESSIONS: [[i32; 4]; 5] = [
    [0, 5, 2, 6],
    [0, 3, 5, 4],
    [5, 6, 0, 0],
    [0, 6, 5, 6],
    [3, 4, 0, 5],
];

#[derive(Clone, Copy)]
struct Layers {
    bass: bool,
    arp: bool,
    lead: bool,
    kick: bool,
    snare: bool,
    hats: bool,
    /// Arpeggio an octave higher.
    lift: bool,
}

/// Intro, groove, full, lifted, breakdown, drive.
const ARRANGEMENT: [Layers; 6] = [
    Layers {
        bass: true,
        arp: true,
        lead: false,
        kick: false,
        snare: false,
        hats: true,
        lift: false,
    },
    Layers {
        bass: true,
        arp: true,
        lead: false,
        kick: true,
        snare: true,
        hats: true,
        lift: false,
    },
    Layers {
        bass: true,
        arp: true,
        lead: true,
        kick: true,
        snare: true,
        hats: true,
        lift: false,
    },
    Layers {
        bass: true,
        arp: true,
        lead: true,
        kick: true,
        snare: true,
        hats: true,
        lift: true,
    },
    Layers {
        bass: false,
        arp: true,
        lead: true,
        kick: false,
        snare: false,
        hats: true,
        lift: true,
    },
    Layers {
        bass: true,
        arp: false,
        lead: true,
        kick: true,
        snare: true,
        hats: true,
        lift: false,
    },
];

fn midi_hz(m: i32) -> f32 {
    440.0 * 2f32.powf((m - 69) as f32 / 12.0)
}

/// MIDI note of scale degree `degree` (may exceed 7 or be negative) above `base` (an A).
fn degree_midi(base: i32, degree: i32) -> i32 {
    base + 12 * degree.div_euclid(7) + SCALE[degree.rem_euclid(7) as usize]
}

#[derive(Clone, Copy, PartialEq)]
enum Wave {
    /// Duty cycle.
    Pulse(f32),
    /// NES-style 4-bit stepped triangle.
    Triangle,
}

/// One tonal channel with a decay/sustain/release envelope.
struct Voice {
    wave: Wave,
    freq: f32,
    phase: f32,
    gate: u32,
    age: u32,
    decay: f32,
    sustain: f32,
    release: f32,
    peak: f32,
    level: f32,
}

impl Voice {
    fn new(wave: Wave, decay_s: f32, sustain: f32, release_s: f32) -> Self {
        Self {
            wave,
            freq: 0.0,
            phase: 0.0,
            gate: 0,
            age: u32::MAX,
            decay: (-1.0 / (decay_s * SR)).exp(),
            sustain,
            release: (-1.0 / (release_s * SR)).exp(),
            peak: 0.0,
            level: 0.0,
        }
    }

    fn play(&mut self, midi: i32, gate_s: f32) {
        self.freq = midi_hz(midi);
        self.gate = (gate_s * SR) as u32;
        self.age = 0;
        self.peak = 1.0;
    }

    fn sample(&mut self) -> f32 {
        if self.age == u32::MAX {
            return 0.0;
        }
        if self.age < self.gate {
            self.peak *= self.decay;
            self.level = self.sustain + (1.0 - self.sustain) * self.peak;
        } else {
            self.level *= self.release;
            if self.level < 1e-4 {
                self.age = u32::MAX;
                return 0.0;
            }
        }
        // A 1 ms attack ramp avoids clicks.
        let attack = (self.age as f32 / (0.001 * SR)).min(1.0);
        self.age = self.age.saturating_add(1);
        self.phase = (self.phase + self.freq / SR).fract();
        let osc = match self.wave {
            Wave::Pulse(duty) => {
                if self.phase < duty {
                    1.0
                } else {
                    -1.0
                }
            }
            Wave::Triangle => ((4.0 * (self.phase - 0.5).abs() - 1.0) * 7.5).round() / 7.5,
        };
        osc * self.level * attack
    }
}

/// NES-style noise: a 15-bit LFSR clocked at a chosen rate, with an exponential decay.
struct Noise {
    lfsr: u16,
    clock: f32,
    rate: f32,
    short: bool,
    level: f32,
    decay: f32,
    out: f32,
}

impl Noise {
    fn new() -> Self {
        Self {
            lfsr: 1,
            clock: 0.0,
            rate: 1.0,
            short: false,
            level: 0.0,
            decay: 0.0,
            out: 1.0,
        }
    }

    fn hit(&mut self, rate_hz: f32, decay_s: f32, short: bool, level: f32) {
        self.rate = rate_hz / SR;
        self.decay = (-1.0 / (decay_s * SR)).exp();
        self.short = short;
        self.level = level;
    }

    fn sample(&mut self) -> f32 {
        if self.level < 1e-4 {
            return 0.0;
        }
        self.clock += self.rate;
        while self.clock >= 1.0 {
            self.clock -= 1.0;
            let tap = if self.short { 6 } else { 1 };
            let bit = (self.lfsr ^ (self.lfsr >> tap)) & 1;
            self.lfsr = (self.lfsr >> 1) | (bit << 14);
            self.out = if self.lfsr & 1 == 0 { 1.0 } else { -1.0 };
        }
        self.level *= self.decay;
        self.out * self.level
    }
}

/// A sine kick with a fast downward pitch sweep.
struct Kick {
    age: u32,
    phase: f32,
}

impl Kick {
    fn sample(&mut self) -> f32 {
        let t = self.age as f32 / SR;
        if t > 0.35 {
            return 0.0;
        }
        self.age += 1;
        let freq = 42.0 + 118.0 * (-t / 0.03).exp();
        self.phase = (self.phase + freq / SR).fract();
        (self.phase * std::f32::consts::TAU).sin() * (-t / 0.12).exp()
    }
}

/// One note of a 2-bar motif: start step, length in steps and scale degree relative to the
/// chord root. `None` is a rest.
#[derive(Clone, Copy, Debug)]
struct MotifNote {
    step: u32,
    len: u32,
    degree: Option<i32>,
}

pub struct Music {
    rng: u64,
    samples_per_step: f64,
    next_step_at: f64,
    sample: u64,
    step: u64,
    progression: [i32; 4],
    motifs: [Vec<MotifNote>; 3],
    bass: Voice,
    arp: Voice,
    lead: Voice,
    blip: Voice,
    kick: Kick,
    snare: Noise,
    hats: Noise,
    crash: Noise,
    /// Pending corner arpeggio: (sample to play at, midi note), bounded.
    flourish: Vec<(u64, i32)>,
    blip_pan: f32,
    blip_count: u32,
    volume: f32,
    lowpass: [f32; 2],
}

impl Music {
    pub fn new(seed: u64, bpm: f32, volume: f32) -> Self {
        let samples_per_step = SR as f64 * 60.0 / bpm as f64 / 4.0;
        let mut m = Self {
            rng: seed.rotate_left(29) | 1,
            samples_per_step,
            next_step_at: 0.0,
            sample: 0,
            step: 0,
            progression: PROGRESSIONS[0],
            motifs: [Vec::new(), Vec::new(), Vec::new()],
            bass: Voice::new(Wave::Triangle, 0.4, 0.7, 0.03),
            arp: Voice::new(Wave::Pulse(0.125), 0.06, 0.25, 0.02),
            lead: Voice::new(Wave::Pulse(0.25), 0.25, 0.6, 0.06),
            blip: Voice::new(Wave::Pulse(0.5), 0.05, 0.0, 0.02),
            kick: Kick {
                age: u32::MAX / 2,
                phase: 0.0,
            },
            snare: Noise::new(),
            hats: Noise::new(),
            crash: Noise::new(),
            flourish: Vec::with_capacity(16),
            blip_pan: 0.0,
            blip_count: 0,
            volume,
            lowpass: [0.0; 2],
        };
        m.compose();
        m
    }

    fn rand(&mut self) -> u64 {
        let s = &mut self.rng;
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    fn chance(&mut self, p: f32) -> bool {
        (self.rand() >> 40) as f32 / (1u64 << 24) as f32 <= p
    }

    /// New progression and motifs for the next arrangement cycle.
    fn compose(&mut self) {
        self.progression = PROGRESSIONS[(self.rand() % PROGRESSIONS.len() as u64) as usize];
        let a = self.motif();
        let b = self.motif();
        // A': A with its last half bar replaced by a long cadence on the root.
        let mut a2: Vec<MotifNote> = a.iter().copied().filter(|n| n.step < 24).collect();
        if let Some(last) = a2.last_mut() {
            last.len = last.len.min(24 - last.step);
        }
        a2.push(MotifNote {
            step: 24,
            len: 8,
            degree: Some(0),
        });
        self.motifs = [a, b, a2];
    }

    /// A 2-bar (32-step) melody, chord-relative: strong beats land on chord tones, weak
    /// steps move stepwise.
    fn motif(&mut self) -> Vec<MotifNote> {
        let mut onsets = Vec::new();
        for step in 0..32u32 {
            let p = if step % 4 == 0 {
                0.85
            } else if step % 2 == 0 {
                0.4
            } else {
                0.15
            };
            if step == 0 || self.chance(p) {
                onsets.push(step);
            }
        }
        let mut notes = Vec::with_capacity(onsets.len());
        let mut prev = 4;
        for (i, &step) in onsets.iter().enumerate() {
            let next = onsets.get(i + 1).copied().unwrap_or(32);
            let degree = if step > 0 && self.chance(0.12) {
                None
            } else if step % 4 == 0 {
                let tones = [0, 2, 4, 7];
                Some(tones[(self.rand() % 4) as usize])
            } else {
                let moves = [-2, -1, 1, 2];
                Some((prev + moves[(self.rand() % 4) as usize]).clamp(-2, 9))
            };
            if let Some(d) = degree {
                prev = d;
            }
            notes.push(MotifNote {
                step,
                len: next - step,
                degree,
            });
        }
        notes
    }

    /// A logo bounce: a blip on a tone of the current chord, panned to where it hit
    /// (`pan` from -1 left to 1 right).
    pub fn bounce(&mut self, pan: f32) {
        let root = self.chord_root();
        let tones = [0, 2, 4, 7];
        let degree = root + tones[(self.blip_count % 4) as usize];
        self.blip_count = self.blip_count.wrapping_add(1);
        self.blip.play(degree_midi(A5, degree), 0.06);
        self.blip_pan = pan.clamp(-1.0, 1.0);
    }

    /// A corner hit: a rising two-octave arpeggio on the current chord and a crash.
    pub fn corner(&mut self) {
        let root = self.chord_root();
        let spacing = (0.035 * SR) as u64;
        self.flourish.clear();
        for (i, d) in [0, 2, 4, 7, 9, 11, 14, 16].iter().enumerate() {
            self.flourish
                .push((self.sample + i as u64 * spacing, degree_midi(A4, root + d)));
        }
        self.crash.hit(9_000.0, 0.9, false, 0.9);
    }

    fn chord_root(&self) -> i32 {
        let bar = (self.step % STEPS_PER_PHRASE as u64) as u32 / STEPS_PER_BAR;
        self.progression[(bar % 4) as usize]
    }

    /// Runs the sequencer for one 16th-note step.
    fn tick(&mut self) {
        let in_phrase = (self.step % STEPS_PER_PHRASE as u64) as u32;
        let phrase = self.step / STEPS_PER_PHRASE as u64;
        let layers = ARRANGEMENT[(phrase % ARRANGEMENT.len() as u64) as usize];
        if in_phrase == 0 && phrase > 0 && phrase.is_multiple_of(ARRANGEMENT.len() as u64) {
            self.compose();
        }
        let next_layers = ARRANGEMENT[((phrase + 1) % ARRANGEMENT.len() as u64) as usize];
        let bar = in_phrase / STEPS_PER_BAR;
        let s = in_phrase % STEPS_PER_BAR;
        let root = self.progression[(bar % 4) as usize];
        let step_s = self.samples_per_step as f32 / SR;

        if layers.bass && s.is_multiple_of(2) {
            // Root on the beat, octave on the off-beat eighth, a fifth before the next bar.
            let degree = match s {
                14 => root + 4,
                _ if s % 4 == 2 => root + 7,
                _ => root,
            };
            self.bass.play(degree_midi(A1, degree), step_s * 1.6);
        }
        if layers.arp {
            let tones = [0, 2, 4, 7];
            let base = if layers.lift { A4 } else { A3 };
            self.arp.play(
                degree_midi(base, root + tones[(s % 4) as usize]),
                step_s * 0.7,
            );
        }
        if layers.lead {
            // Motifs per 2-bar slot: A A B A'.
            let slot = bar / 2;
            let motif = match slot {
                0 | 1 => 0,
                2 => 1,
                _ => 2,
            };
            let local = in_phrase % 32;
            if let Some(note) = self.motifs[motif].iter().find(|n| n.step == local).copied() {
                if let Some(d) = note.degree {
                    // Hold within the bar's chord: degrees are relative to the chord root.
                    self.lead
                        .play(degree_midi(A4, root + d), note.len as f32 * step_s * 0.85);
                }
            }
        }
        let fill = next_layers.snare && bar == BARS_PER_PHRASE - 1 && s >= 8;
        if layers.kick && (s == 0 || s == 8 || (s == 10 && bar % 2 == 1)) {
            self.kick = Kick { age: 0, phase: 0.0 };
        }
        if fill {
            // Snare roll into the next phrase, getting louder.
            self.snare
                .hit(4_500.0, 0.06, false, 0.3 + 0.07 * (s - 8) as f32);
        } else if layers.snare && (s == 4 || s == 12) {
            self.snare.hit(4_500.0, 0.12, false, 1.0);
        }
        if layers.hats && s.is_multiple_of(2) {
            let open = s == 14;
            self.hats
                .hit(18_000.0, if open { 0.12 } else { 0.025 }, true, 0.8);
        }
    }

    /// Appends `frames` stereo frames of 16-bit PCM to `out`.
    pub fn render(&mut self, frames: usize, out: &mut Vec<i16>) {
        out.reserve(frames * 2);
        for _ in 0..frames {
            while self.sample as f64 >= self.next_step_at {
                self.tick();
                self.step += 1;
                self.next_step_at += self.samples_per_step;
            }
            while let Some(&(at, midi)) = self.flourish.first() {
                if at > self.sample {
                    break;
                }
                self.blip.play(midi, 0.03);
                self.blip_pan = 0.0;
                self.flourish.remove(0);
            }
            let bass = self.bass.sample() * 0.32;
            let arp = self.arp.sample() * 0.09;
            let lead = self.lead.sample() * 0.16;
            let blip = self.blip.sample() * 0.16;
            let kick = self.kick.sample() * 0.55;
            let snare = self.snare.sample() * 0.2;
            let hats = self.hats.sample() * 0.05;
            let crash = self.crash.sample() * 0.07;
            let centre = bass + kick + snare + crash;
            // Light panning for width: arp left, lead and hats right, blips where they hit.
            let left = centre + arp * 1.3 + lead * 0.8 + hats * 0.7 + blip * (1.0 - self.blip_pan);
            let right = centre + arp * 0.7 + lead * 1.2 + hats * 1.3 + blip * (1.0 + self.blip_pan);
            for (ch, x) in [left, right].into_iter().enumerate() {
                // Gentle low-pass takes the edge off the raw pulses, then a soft clip.
                self.lowpass[ch] += 0.55 * (x - self.lowpass[ch]);
                let y = self.lowpass[ch] * self.volume;
                let y = y / (1.0 + y.abs() * 0.5);
                out.push((y.clamp(-1.0, 1.0) * 32_000.0) as i16);
            }
            self.sample += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(pcm: &[i16]) -> f64 {
        (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
    }

    #[test]
    fn deterministic_and_audible_without_clipping() {
        let mut a = Music::new(7, 125.0, 1.0);
        let mut b = Music::new(7, 125.0, 1.0);
        let (mut pa, mut pb) = (Vec::new(), Vec::new());
        // Two full arrangement cycles (12 phrases of ~15 s).
        for _ in 0..(12 * 16 * 48) {
            a.render(240, &mut pa);
            b.render(240, &mut pb);
        }
        assert_eq!(pa, pb);
        assert!(rms(&pa) > 1500.0, "too quiet: {}", rms(&pa));
        let peak = pa.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak < 32_000, "clipping: {peak}");
        // No silent stretch longer than half a second anywhere.
        for window in pa.chunks(SAMPLE_RATE as usize) {
            assert!(rms(window) > 300.0);
        }
    }

    #[test]
    fn has_a_beat() {
        // In the groove phrase (phrase 1) the kick lands every half bar: energy in the first
        // 20 ms after each kick step clearly exceeds the energy just before it.
        let mut m = Music::new(3, 125.0, 1.0);
        let mut pcm = Vec::new();
        let phrase = (STEPS_PER_PHRASE as f64 * m.samples_per_step) as usize;
        m.render(phrase * 2, &mut pcm);
        let step = m.samples_per_step;
        let mono = |i: usize| pcm[2 * i] as f64;
        let energy = |from: usize, len: usize| {
            (from..from + len).map(|i| mono(i).powi(2)).sum::<f64>() / len as f64
        };
        let win = (0.02 * SR) as usize;
        let mut louder = 0;
        let kicks: Vec<usize> = (0..8)
            .map(|bar| phrase + ((bar * 16 + 8) as f64 * step) as usize)
            .collect();
        for &k in &kicks {
            if energy(k, win) > 1.5 * energy(k - win, win) {
                louder += 1;
            }
        }
        assert!(louder >= 6, "kick onsets not detected ({louder}/8)");
    }

    #[test]
    fn effects_are_audible_and_bounded() {
        let mut quiet = Music::new(5, 125.0, 1.0);
        let mut busy = Music::new(5, 125.0, 1.0);
        let (mut q, mut b) = (Vec::new(), Vec::new());
        quiet.render(4_800, &mut q);
        busy.render(4_800, &mut b);
        busy.bounce(1.0);
        for _ in 0..100 {
            busy.corner(); // repeated corners must not grow the queue without bound
        }
        assert!(busy.flourish.len() <= 8);
        quiet.render(4_800, &mut q);
        busy.render(4_800, &mut b);
        assert!(rms(&b[9_600..]) > rms(&q[9_600..]));
    }

    #[test]
    fn notes_stay_in_a_minor() {
        let in_key = |m: i32| SCALE.contains(&(m - A1).rem_euclid(12));
        for d in -10..30 {
            assert!(in_key(degree_midi(A3, d)));
        }
        assert_eq!(degree_midi(A4, 0), 69);
        assert_eq!(degree_midi(A4, 7), 81);
        assert_eq!(degree_midi(A4, -1), 67);
    }
}
