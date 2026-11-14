//! KG3N: pure DSP / scheduling logic for the controlled-side "system audio + microphone" mix
//! (option `audio-mix-mic`). Deliberately free of cpal / opus / hbb_common so it can be
//! unit-tested on its own. `audio_service.rs` does the device and thread wiring.
//!
//! Pipeline per source (in the capture callback):
//!     device samples -> to_stereo (at the device rate) -> StreamResampler (stateful) -> SourceBuf
//! Mixer thread:
//!     Mixer::next_frame (clocked by the sources themselves) -> Dsp::process (gains, ducking,
//!     soft limiter, noise gate) -> opus encoder (in audio_service.rs)
//!
//! Fixes over the first version of the mix:
//! - Frames are emitted only when every source that is still delivering has a full 10 ms, so the
//!   two capture clocks no longer produce extra, half-silent frames (stutter + growing latency).
//! - A live source that runs ahead (clock drift) is trimmed back, bounding latency.
//! - Channels are converted BEFORE resampling, and resampling keeps its phase between callbacks
//!   (no per-chunk edge clicks, no lost fractional samples).
//! - Soft-knee limiter instead of a hard clamp; configurable gains; optional ducking; a noise gate
//!   so a silent session stops streaming (the mic never outputs exact zeros on its own).

use std::collections::VecDeque;

/// Mix format: 48 kHz interleaved stereo.
pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// One opus frame: 10 ms of 48 kHz stereo = 960 interleaved samples.
pub const FRAME: usize = 960;
/// A source that delivered within this window is "live" and must be waited for.
pub const LIVE_MS: u64 = 40;
/// Latency cap for a live source (8 frames = 80 ms); beyond it the oldest audio is dropped.
pub const MAX_LAG: usize = FRAME * 8;
/// What is kept after such a trim (4 frames = 40 ms).
pub const KEEP_AFTER_TRIM: usize = FRAME * 4;
/// Absolute buffer cap (2 s), in case the mixer thread is not draining at all.
pub const MAX_BUF: usize = RATE as usize * CHANNELS * 2;

/// Interleaved `channels`-channel samples -> interleaved stereo. Mono is copied to both sides;
/// more than two channels (mic arrays, 5.1/7.1 loopback) average even-indexed channels into L
/// and odd-indexed ones into R. A trailing partial frame is ignored.
pub fn to_stereo(data: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        0 => Vec::new(),
        1 => {
            let mut out = Vec::with_capacity(data.len() * 2);
            for s in data {
                out.push(*s);
                out.push(*s);
            }
            out
        }
        2 => data[..data.len() - data.len() % 2].to_vec(),
        n => {
            let frames = data.len() / n;
            let n_l = (n + 1) / 2; // even indices 0,2,4..
            let n_r = n / 2; // odd indices 1,3,5..
            let mut out = Vec::with_capacity(frames * 2);
            for f in 0..frames {
                let fr = &data[f * n..(f + 1) * n];
                let (mut l, mut r) = (0f32, 0f32);
                for (i, s) in fr.iter().enumerate() {
                    if i % 2 == 0 {
                        l += *s;
                    } else {
                        r += *s;
                    }
                }
                out.push(l / n_l as f32);
                out.push(r / n_r as f32);
            }
            out
        }
    }
}

/// Stateful linear resampler for interleaved stereo. Unlike resampling each callback chunk on its
/// own, it carries the fractional position and the previous frame across calls, so chunk edges
/// are continuous and no fractional samples are lost (the long-run output rate is exact).
pub struct StreamResampler {
    step: f64, // input frames advanced per output frame
    pos: f64,  // position of the next output frame, in input frames, relative to `prev`
    prev: [f32; 2],
    primed: bool,
    passthrough: bool,
}

impl StreamResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        let from = from_rate.max(1);
        let to = to_rate.max(1);
        Self {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: [0.0; 2],
            primed: false,
            passthrough: from == to,
        }
    }

    /// `input`: interleaved stereo at the source rate. Returns interleaved stereo at the target rate.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let n = input.len() / 2;
        if self.passthrough {
            return input[..n * 2].to_vec();
        }
        if n == 0 {
            return Vec::new();
        }
        if !self.primed {
            self.prev = [input[0], input[1]];
            self.primed = true;
        }
        // Virtual sequence: s[0] = prev, s[1..=n] = input frames. Interpolate between s[i], s[i+1].
        let at = |i: usize, c: usize, prev: &[f32; 2]| -> f32 {
            if i == 0 {
                prev[c]
            } else {
                input[(i - 1) * 2 + c]
            }
        };
        let mut out = Vec::with_capacity(((n as f64 / self.step) as usize + 2) * 2);
        let mut t = self.pos;
        while t < n as f64 {
            let i = t.floor() as usize;
            let frac = (t - i as f64) as f32;
            for c in 0..2 {
                let a = at(i, c, &self.prev);
                let b = at(i + 1, c, &self.prev);
                out.push(a + (b - a) * frac);
            }
            t += self.step;
        }
        self.pos = t - n as f64;
        self.prev = [input[(n - 1) * 2], input[(n - 1) * 2 + 1]];
        out
    }
}

/// One source's FIFO (interleaved stereo at 48 kHz) plus when it last delivered audio.
#[derive(Default)]
pub struct SourceBuf {
    buf: VecDeque<f32>,
    last_push_ms: Option<u64>,
}

impl SourceBuf {
    pub fn push(&mut self, samples: &[f32], now_ms: u64) {
        let even = samples.len() - samples.len() % 2;
        self.buf.extend(&samples[..even]);
        if self.buf.len() > MAX_BUF {
            let excess = (self.buf.len() - MAX_BUF + 1) & !1;
            self.buf.drain(0..excess.min(self.buf.len()));
        }
        self.last_push_ms = Some(now_ms);
    }
    pub fn len(&self) -> usize {
        self.buf.len()
    }
    pub fn is_live(&self, now_ms: u64) -> bool {
        self.last_push_ms
            .map(|t| now_ms.saturating_sub(t) <= LIVE_MS)
            .unwrap_or(false)
    }
    /// Milliseconds since this source last delivered, or None if it never did.
    pub fn age_ms(&self, now_ms: u64) -> Option<u64> {
        self.last_push_ms.map(|t| now_ms.saturating_sub(t))
    }
    fn trim_to(&mut self, keep: usize) {
        if self.buf.len() > keep {
            let drop = (self.buf.len() - keep) & !1;
            self.buf.drain(0..drop);
        }
    }
    /// Take exactly `n` samples, zero-padding if fewer are buffered. Returns (samples, padded).
    fn take(&mut self, n: usize) -> (Vec<f32>, bool) {
        let k = self.buf.len().min(n);
        let mut v: Vec<f32> = self.buf.drain(0..k).collect();
        let padded = k < n;
        v.resize(n, 0.0);
        (v, padded)
    }
    pub fn clear(&mut self) {
        self.buf.clear();
        self.last_push_ms = None;
    }
}

/// A frame ready for the DSP stage, with diagnostics.
pub struct MixFrame {
    pub sys: Vec<f32>,
    pub mic: Vec<f32>,
    /// true when that source contributed less than a full frame (it went idle or ended)
    pub sys_padded: bool,
    pub mic_padded: bool,
}

/// Clocks the mix from the sources themselves: a frame is emitted only when every LIVE source
/// has a full frame; an idle source (e.g. loopback while nothing plays) is padded with silence.
#[derive(Default)]
pub struct Mixer {
    pub sys: SourceBuf,
    pub mic: SourceBuf,
    /// samples dropped by drift/latency trimming (diagnostics)
    pub trimmed: u64,
}

impl Mixer {
    pub fn clear(&mut self) {
        self.sys.clear();
        self.mic.clear();
        self.trimmed = 0;
    }

    pub fn next_frame(&mut self, now_ms: u64) -> Option<MixFrame> {
        // latency cap / drift correction: a source that ran ahead is trimmed back
        for b in [&mut self.sys, &mut self.mic] {
            if b.len() > MAX_LAG {
                let before = b.len();
                b.trim_to(KEEP_AFTER_TRIM);
                self.trimmed += (before - b.len()) as u64;
            }
        }
        let (s_ready, m_ready) = (self.sys.len() >= FRAME, self.mic.len() >= FRAME);
        let (s_live, m_live) = (self.sys.is_live(now_ms), self.mic.is_live(now_ms));
        let go = (s_ready || m_ready) && (s_ready || !s_live) && (m_ready || !m_live);
        // both sources stopped: flush whatever partial audio is left, once
        let flush = !s_live && !m_live && (self.sys.len() > 0 || self.mic.len() > 0);
        if !go && !flush {
            return None;
        }
        let (sys, sys_padded) = self.sys.take(FRAME);
        let (mic, mic_padded) = self.mic.take(FRAME);
        Some(MixFrame {
            sys,
            mic,
            sys_padded,
            mic_padded,
        })
    }
}

#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Parse a dB option value; empty/invalid -> `default`; result clamped to [lo, hi].
pub fn parse_db(s: &str, default: f32, lo: f32, hi: f32) -> f32 {
    s.trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .unwrap_or(default)
        .clamp(lo, hi)
}

/// Soft-knee limiter: transparent below 0.8 (about -2 dBFS), smoothly approaches 1.0 above.
#[inline]
pub fn soft_limit(x: f32) -> f32 {
    const T: f32 = 0.8;
    let a = x.abs();
    if a <= T {
        x
    } else {
        x.signum() * (T + (1.0 - T) * ((a - T) / (1.0 - T)).tanh())
    }
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// Operator-tunable mix parameters (from the audio-mix-* options).
#[derive(Clone, Copy, Debug)]
pub struct MixParams {
    pub sys_gain: f32,
    pub mic_gain: f32,
    /// multiplier applied to system audio while the mic is active; 1.0 = ducking off
    pub duck_gain: f32,
    /// output RMS below this (linear) for the hold time becomes digital silence; 0.0 = gate off
    pub gate_threshold: f32,
}

impl MixParams {
    /// From the option strings: mic gain dB (default 0, -24..+24), system gain dB (default -3,
    /// -24..+12), duck dB (default 0 = off, 0..30), gate dB (default -55; >= 0 = off, floor -90).
    pub fn from_options(mic_db: &str, sys_db: &str, duck_db: &str, gate_db: &str) -> Self {
        let mic = parse_db(mic_db, 0.0, -24.0, 24.0);
        let sys = parse_db(sys_db, -3.0, -24.0, 12.0);
        let duck = parse_db(duck_db, 0.0, 0.0, 30.0);
        let gate = parse_db(gate_db, -55.0, -90.0, 0.0);
        Self {
            sys_gain: db_to_lin(sys),
            mic_gain: db_to_lin(mic),
            duck_gain: if duck <= 0.0 { 1.0 } else { db_to_lin(-duck) },
            gate_threshold: if gate >= 0.0 { 0.0 } else { db_to_lin(gate) },
        }
    }
}

impl Default for MixParams {
    fn default() -> Self {
        Self::from_options("", "", "", "")
    }
}

/// Mic level (after gain) that counts as "speaking" for ducking: about -40 dBFS RMS.
const DUCK_TRIGGER: f32 = 0.01;
/// Gate hold: stay open this many frames (300 ms) after the last frame above the threshold.
const GATE_HOLD_FRAMES: u32 = 30;

/// Per-stream DSP state: ducking envelope + gate hold.
pub struct Dsp {
    p: MixParams,
    duck_env: f32,
    gate_hold: u32,
    attack: f32,
    release: f32,
}

impl Dsp {
    pub fn new(p: MixParams) -> Self {
        // per-10 ms-frame smoothing coefficients: ~10 ms attack, ~300 ms release
        Self {
            p,
            duck_env: 1.0,
            gate_hold: 0,
            attack: 1.0 - (-10.0f32 / 10.0).exp(),
            release: 1.0 - (-10.0f32 / 300.0).exp(),
        }
    }

    /// Mix one frame. `sys` and `mic` must have the same length (FRAME).
    pub fn process(&mut self, sys: &[f32], mic: &[f32]) -> Vec<f32> {
        let p = self.p;
        if p.duck_gain < 1.0 {
            let target = if rms(mic) * p.mic_gain > DUCK_TRIGGER {
                p.duck_gain
            } else {
                1.0
            };
            let k = if target < self.duck_env { self.attack } else { self.release };
            self.duck_env += (target - self.duck_env) * k;
        } else {
            self.duck_env = 1.0;
        }
        let g_sys = p.sys_gain * self.duck_env;
        let mut out: Vec<f32> = sys
            .iter()
            .zip(mic.iter())
            .map(|(s, m)| soft_limit(s * g_sys + m * p.mic_gain))
            .collect();
        if p.gate_threshold > 0.0 {
            if rms(&out) >= p.gate_threshold {
                self.gate_hold = GATE_HOLD_FRAMES;
            } else if self.gate_hold > 0 {
                self.gate_hold -= 1;
            }
            if self.gate_hold == 0 {
                out.iter_mut().for_each(|v| *v = 0.0);
            }
        }
        out
    }
}

/// Is `name` a loopback-style capture device ("Stereo Mix" etc.)? Mixing it with system audio
/// would play the system audio twice.
pub fn is_loopback_input_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["stereo mix", "what u hear", "wave out", "loopback", "what you hear"]
        .iter()
        .any(|k| n.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_stereo(rate: u32, hz: f32, frames: usize, start: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * 2);
        for i in start..start + frames {
            let s = (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5;
            v.push(s);
            v.push(s);
        }
        v
    }

    #[test]
    fn stereo_conversion() {
        assert_eq!(to_stereo(&[0.1, 0.2], 1), vec![0.1, 0.1, 0.2, 0.2]);
        assert_eq!(to_stereo(&[0.1, 0.2, 0.3, 0.4, 0.9], 2), vec![0.1, 0.2, 0.3, 0.4]);
        // 4 channels: L = mean(ch0, ch2), R = mean(ch1, ch3)
        let v = to_stereo(&[0.2, 0.4, 0.6, 0.8], 4);
        assert!((v[0] - 0.4).abs() < 1e-6 && (v[1] - 0.6).abs() < 1e-6);
        // 6 channels (5.1) gives one stereo frame per input frame
        assert_eq!(to_stereo(&vec![0.5; 6 * 10], 6).len(), 20);
        assert!(to_stereo(&[0.5; 3], 0).is_empty());
    }

    #[test]
    fn resampler_long_run_rate_is_exact() {
        // 1 s of 44.1 kHz in uneven chunk sizes -> ~48000 output frames (no fractional loss)
        let mut r = StreamResampler::new(44_100, 48_000);
        let chunks = [441usize, 448, 100, 1, 973, 512];
        let (mut fed, mut out, mut i) = (0usize, 0usize, 0usize);
        while fed < 44_100 {
            let n = chunks[i % chunks.len()].min(44_100 - fed);
            out += r.process(&vec![0.1; n * 2]).len() / 2;
            fed += n;
            i += 1;
        }
        assert!((out as i64 - 48_000).abs() <= 2, "got {} frames", out);
        // 16 kHz -> 48 kHz exact x3
        let mut r = StreamResampler::new(16_000, 48_000);
        let mut out = 0;
        for _ in 0..100 {
            out += r.process(&vec![0.0; 160 * 2]).len() / 2;
        }
        assert!((out as i64 - 48_000).abs() <= 3, "got {}", out);
    }

    #[test]
    fn resampler_no_chunk_edge_clicks_16k() {
        // 1 kHz sine at 16 kHz in 160-frame (10 ms) callbacks: max step between consecutive output
        // samples must stay near the sine's true max slope (no pull-to-zero at chunk edges)
        let mut r = StreamResampler::new(16_000, 48_000);
        let mut out = Vec::new();
        for c in 0..50 {
            out.extend(r.process(&sine_stereo(16_000, 1000.0, 160, c * 160)));
        }
        let l: Vec<f32> = out.iter().step_by(2).cloned().collect();
        let max_jump = l.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        // true max per-sample change of 0.5*sin(2*pi*1000*t) at 48 kHz ~= 0.0654
        assert!(max_jump < 0.09, "max jump {}", max_jump);
    }

    #[test]
    fn resampler_passthrough() {
        let mut r = StreamResampler::new(48_000, 48_000);
        let v = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(r.process(&v), v);
    }

    /// Simulates two capture devices: each delivers `chunk` stereo frames every `period_us`,
    /// with its own phase offset. The mixer thread polls every 2 ms. Returns
    /// (frames emitted, frames where a LIVE source was zero-padded, max buffer length).
    fn simulate(
        secs: u64,
        sys: Option<(u64, u64, usize)>, // (period_us, offset_us, chunk_frames)
        mic: Option<(u64, u64, usize)>,
    ) -> (u64, u64, usize, u64) {
        let mut m = Mixer::default();
        let end_us = secs * 1_000_000;
        let (mut next_s, mut next_m) = (sys.map(|s| s.1), mic.map(|s| s.1));
        let mut next_poll = 0u64;
        let (mut frames, mut bad, mut maxlen) = (0u64, 0u64, 0usize);
        let mut t = 0u64;
        while t < end_us {
            let mut nt = next_poll;
            if let Some(x) = next_s { nt = nt.min(x); }
            if let Some(x) = next_m { nt = nt.min(x); }
            t = nt;
            let now_ms = t / 1000;
            if Some(t) == next_s {
                let (p, _, c) = sys.unwrap();
                m.sys.push(&vec![0.1; c * 2], now_ms);
                next_s = Some(t + p);
            }
            if Some(t) == next_m {
                let (p, _, c) = mic.unwrap();
                m.mic.push(&vec![0.2; c * 2], now_ms);
                next_m = Some(t + p);
            }
            if t == next_poll {
                while let Some(f) = m.next_frame(now_ms) {
                    frames += 1;
                    let s_live_and_padded = f.sys_padded && sys.is_some();
                    let m_live_and_padded = f.mic_padded && mic.is_some();
                    if s_live_and_padded || m_live_and_padded {
                        bad += 1;
                    }
                }
                next_poll = t + 2000;
            }
            maxlen = maxlen.max(m.sys.len()).max(m.mic.len());
        }
        (frames, bad, maxlen, m.trimmed)
    }

    #[test]
    fn mixer_two_live_sources_emit_100_frames_per_second_without_padding() {
        // both devices deliver 10 ms chunks, 5 ms apart: the old mixer sent 110-175 frames/s
        let (frames, bad, maxlen, _) =
            simulate(10, Some((10_000, 0, 480)), Some((10_000, 5_000, 480)));
        assert!((frames as i64 - 1000).abs() <= 2, "frames {}", frames);
        assert_eq!(bad, 0, "padded frames while both live");
        assert!(maxlen <= MAX_LAG, "maxlen {}", maxlen);
        // odd 3 ms packets on one side
        let (frames, bad, _, _) = simulate(10, Some((3_000, 0, 144)), Some((10_000, 1_000, 480)));
        assert!((frames as i64 - 1000).abs() <= 2, "frames {}", frames);
        assert_eq!(bad, 0);
    }

    #[test]
    fn mixer_single_source_runs_at_100_per_second() {
        // loopback silent (no packets at all): mic alone drives the clock
        let (frames, bad, _, _) = simulate(5, None, Some((10_000, 0, 480)));
        assert!((frames as i64 - 500).abs() <= 2, "frames {}", frames);
        assert_eq!(bad, 0);
        let (frames, _, _, _) = simulate(5, Some((10_000, 0, 480)), None);
        assert!((frames as i64 - 500).abs() <= 2, "frames {}", frames);
    }

    #[test]
    fn mixer_bounds_latency_under_clock_drift() {
        // sys device 0.1% fast vs mic: buffers must stay bounded (trimming), rate ~ the slower clock
        let (frames, bad, maxlen, trimmed) =
            simulate(60, Some((9_990, 0, 480)), Some((10_000, 4_000, 480)));
        assert!(maxlen <= MAX_LAG + 960, "maxlen {}", maxlen);
        assert!(trimmed > 0, "expected drift trimming");
        assert!((frames as i64 - 6000).abs() <= 10, "frames {}", frames);
        assert_eq!(bad, 0);
    }

    #[test]
    fn mixer_flushes_and_handles_source_stopping() {
        let mut m = Mixer::default();
        m.sys.push(&vec![0.1; 300], 0);
        m.mic.push(&vec![0.1; 960], 0);
        assert!(m.next_frame(10).is_none(), "sys is live but not ready: wait");
        let f = m.next_frame(100).expect("both idle now: flush"); // 100 ms later
        assert!(f.sys_padded && !f.mic_padded);
        assert!(m.next_frame(101).is_none());
    }

    #[test]
    fn soft_limiter() {
        assert_eq!(soft_limit(0.5), 0.5);
        assert_eq!(soft_limit(-0.8), -0.8);
        for x in [0.9f32, 1.0, 1.5, 2.0, 10.0, -3.0] {
            let y = soft_limit(x);
            assert!(y.abs() < 1.0 && y.abs() > 0.8 && y.signum() == x.signum(), "{} -> {}", x, y);
        }
        assert!(soft_limit(1.2) < soft_limit(1.5)); // monotonic
    }

    #[test]
    fn params_parse_and_clamp() {
        let p = MixParams::default();
        assert!((p.sys_gain - db_to_lin(-3.0)).abs() < 1e-6);
        assert!((p.mic_gain - 1.0).abs() < 1e-6);
        assert_eq!(p.duck_gain, 1.0);
        assert!((p.gate_threshold - db_to_lin(-55.0)).abs() < 1e-9);
        let p = MixParams::from_options("100", "junk", "8", "0");
        assert!((p.mic_gain - db_to_lin(24.0)).abs() < 1e-3); // clamped
        assert!((p.sys_gain - db_to_lin(-3.0)).abs() < 1e-6); // invalid -> default
        assert!((p.duck_gain - db_to_lin(-8.0)).abs() < 1e-6);
        assert_eq!(p.gate_threshold, 0.0); // 0 dB = gate off
    }

    #[test]
    fn dsp_gate_zeroes_silence_and_opens_instantly() {
        let mut d = Dsp::new(MixParams::default());
        let quiet = vec![0.0003f32; FRAME]; // ~-70 dBFS hiss
        let loud = vec![0.2f32; FRAME];
        let z = vec![0f32; FRAME];
        assert!(d.process(&z, &quiet).iter().all(|v| *v == 0.0), "hiss gated");
        assert!(d.process(&loud, &z).iter().any(|v| *v != 0.0), "opens on the first loud frame");
        for _ in 0..GATE_HOLD_FRAMES - 1 {
            assert!(d.process(&z, &quiet).iter().any(|v| *v != 0.0), "held open");
        }
        assert!(d.process(&z, &quiet).iter().all(|v| *v == 0.0), "closes after the hold");
        let mut off = Dsp::new(MixParams::from_options("", "", "", "0"));
        assert!(off.process(&z, &quiet).iter().any(|v| *v != 0.0), "gate off passes hiss");
    }

    #[test]
    fn dsp_gains_limit_and_ducking() {
        let p = MixParams::from_options("6", "0", "10", "0");
        let mut d = Dsp::new(p);
        let sys = vec![0.5f32; FRAME];
        let speech = vec![0.1f32; FRAME];
        let z = vec![0f32; FRAME];
        let first = d.process(&sys, &z)[0];
        assert!((first - 0.5).abs() < 1e-3, "no duck without speech: {}", first);
        for _ in 0..20 {
            d.process(&sys, &speech);
        }
        let ducked = d.process(&sys, &z.clone())[0]; // release has only just started
        let mut d2 = Dsp::new(p);
        let mix = d2.process(&sys, &speech)[0];
        assert!(mix.abs() <= 1.0);
        assert!(ducked < 0.45, "system ducked while speaking: {}", ducked);
        for _ in 0..200 {
            d.process(&sys, &z);
        }
        assert!((d.process(&sys, &z)[0] - 0.5).abs() < 0.01, "recovers after release");
        let big = Dsp::new(MixParams::from_options("24", "12", "", "0")).process(&vec![0.9; FRAME], &vec![0.9; FRAME]);
        assert!(big.iter().all(|v| v.abs() < 1.0), "limiter keeps hot mixes in range");
    }

    #[test]
    fn loopback_names() {
        assert!(is_loopback_input_name("Stereo Mix (Realtek(R) Audio)"));
        assert!(is_loopback_input_name("What U Hear (Sound Blaster)"));
        assert!(!is_loopback_input_name("Microphone Array (Intel Smart Sound)"));
    }
}
