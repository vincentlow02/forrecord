//! Polyphase FIR and downmix adapted from MeetRec's src/capture/mod.rs (MIT).

use cpal::{FromSample, Sample, SizedSample};
use std::f64::consts::PI;

const TAPS_PER_PHASE: usize = 128;

fn f32_to_i16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

pub fn downmix_to_mono_f32<T: SizedSample>(data: &[T], channels: u16) -> Vec<f32>
where
    f32: FromSample<T>,
{
    let channels = usize::from(channels.max(1));
    data.chunks_exact(channels)
        .map(|frame| {
            let sum: f32 = frame.iter().map(|&sample| f32::from_sample(sample)).sum();
            sum / channels as f32
        })
        .collect()
}

pub struct Resampler {
    up: usize,
    down: usize,
    taps: Vec<f32>,
    buffer: Vec<f32>,
    base: usize,
    phase: usize,
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Create a low-pass windowed sinc filter with unity gain at DC.
fn design_lowpass(num_taps: usize, cutoff: f64) -> Vec<f32> {
    let center = (num_taps - 1) as f64 / 2.0;
    let mut coefficients: Vec<f64> = (0..num_taps)
        .map(|i| {
            let x = i as f64 - center;
            let sinc = if x.abs() < 1e-12 {
                2.0 * cutoff
            } else {
                (2.0 * PI * cutoff * x).sin() / (PI * x)
            };
            let n = i as f64 / (num_taps - 1) as f64;
            let window = 0.42 - 0.5 * (2.0 * PI * n).cos() + 0.08 * (4.0 * PI * n).cos();
            sinc * window
        })
        .collect();
    let sum: f64 = coefficients.iter().sum();
    if sum.abs() > 1e-12 {
        for coefficient in &mut coefficients {
            *coefficient /= sum;
        }
    }
    coefficients.into_iter().map(|value| value as f32).collect()
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        if from_rate == to_rate {
            return Self {
                up: 1,
                down: 1,
                taps: vec![],
                buffer: vec![],
                base: 0,
                phase: 0,
            };
        }
        let divisor = gcd(from_rate as usize, to_rate as usize);
        let up = to_rate as usize / divisor;
        let down = from_rate as usize / divisor;
        let cutoff = 0.5 / up.max(down) as f64;
        Self {
            up,
            down,
            taps: design_lowpass(TAPS_PER_PHASE * up, cutoff),
            buffer: vec![0.0; TAPS_PER_PHASE - 1],
            base: TAPS_PER_PHASE - 1,
            phase: 0,
        }
    }

    pub fn reset(&mut self) {
        let history = if self.taps.is_empty() {
            0
        } else {
            TAPS_PER_PHASE - 1
        };
        self.buffer.clear();
        self.buffer.resize(history, 0.0);
        self.base = history;
        self.phase = 0;
    }

    pub fn process_to_i16(&mut self, input: &[f32]) -> Vec<i16> {
        if self.taps.is_empty() {
            return input.iter().copied().map(f32_to_i16).collect();
        }
        self.buffer.extend_from_slice(input);
        let mut output = Vec::with_capacity(input.len() * self.up / self.down + 1);
        // One phase per output sample avoids materializing the interpolated zeros.
        while self.base < self.buffer.len() {
            let mut value = 0.0;
            for tap in 0..TAPS_PER_PHASE {
                value += self.taps[self.phase + tap * self.up] * self.buffer[self.base - tap];
            }
            output.push(f32_to_i16(value * self.up as f32));
            let next = self.phase + self.down;
            self.base += next / self.up;
            self.phase = next % self.up;
        }
        let keep_from = self
            .base
            .saturating_sub(TAPS_PER_PHASE - 1)
            .min(self.buffer.len());
        self.buffer.drain(..keep_from);
        self.base -= keep_from;
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f64, rate: u32) -> Vec<f32> {
        (0..rate)
            .map(|i| (2.0 * PI * frequency * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    #[test]
    fn downmixes_native_integer_samples() {
        assert_eq!(
            downmix_to_mono_f32(&[16_384i16, 0, -16_384, 0], 2),
            [0.25, -0.25]
        );
        assert!(downmix_to_mono_f32::<f32>(&[], 2).is_empty());
    }

    #[test]
    fn speech_band_is_preserved_at_both_common_input_rates() {
        for rate in [44_100, 48_000] {
            let signal = sine(440.0, rate);
            let output = Resampler::new(rate, 16_000).process_to_i16(&signal);
            assert_eq!(output.len(), 16_000);
            let steady = &output[200..output.len() - 200];
            let crossings = steady
                .windows(2)
                .filter(|pair| pair[0] <= 0 && pair[1] > 0)
                .count() as f64;
            let frequency = crossings * 16_000.0 / steady.len() as f64;
            assert!(
                (frequency - 440.0).abs() < 2.0,
                "{rate} Hz input: {frequency} Hz output"
            );
            let energy: f64 = steady
                .iter()
                .map(|&sample| (f64::from(sample) / 32_767.0).powi(2))
                .sum();
            let rms = (energy / steady.len() as f64).sqrt();
            assert!((0.69..0.73).contains(&rms), "{rate} Hz input: RMS {rms}");
        }
    }

    #[test]
    fn rejects_frequencies_above_the_new_nyquist_limit() {
        let signal = sine(12_000.0, 48_000);
        let output = Resampler::new(48_000, 16_000).process_to_i16(&signal);
        assert!(
            output[100..]
                .iter()
                .all(|&sample| sample.unsigned_abs() < 300)
        );
    }

    #[test]
    fn buffer_boundaries_preserve_filter_phase_and_reset_removes_history() {
        let signal = sine(440.0, 44_100);
        let mut streaming = Resampler::new(44_100, 16_000);
        let output: Vec<_> = signal
            .chunks(733)
            .flat_map(|chunk| streaming.process_to_i16(chunk))
            .collect();
        assert_eq!(
            output,
            Resampler::new(44_100, 16_000).process_to_i16(&signal)
        );
        streaming.reset();
        assert_eq!(streaming.process_to_i16(&[0.0; 441]), vec![0; 160]);
    }
}
