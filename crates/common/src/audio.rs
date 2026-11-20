//! Sound shared by host and client: rate and channel conversion around the Opus stream
//! (48 kHz stereo, see [`rdesk_proto::AUDIO_RATE`]).

/// Interleaved samples with `channels` per frame → stereo (mono is doubled, extra
/// channels after front left/right are dropped).
pub fn to_stereo(samples: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend(samples.iter().flat_map(|&s| [s, s])),
        2 => out.extend_from_slice(samples),
        n => {
            for f in samples.chunks_exact(n) {
                out.extend_from_slice(&f[..2]);
            }
        }
    }
}

/// Stereo → `channels` per frame (mono averages; extra channels stay silent).
pub fn from_stereo(samples: &[f32], channels: usize, out: &mut Vec<f32>) {
    for f in samples.as_chunks::<2>().0 {
        match channels {
            0 => {}
            1 => out.push((f[0] + f[1]) * 0.5),
            n => {
                out.extend_from_slice(f);
                out.extend(std::iter::repeat_n(0.0, n - 2));
            }
        }
    }
}

/// Linear resampling of interleaved stereo, chunk by chunk (keeps its place between
/// chunks). Good enough for a remote desktop's sound; the same rate passes untouched.
pub struct Resampler {
    from: u32,
    to: u32,
    /// Position of the next output frame, in input frames, relative to `prev`.
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { from: from.max(1), to: to.max(1), pos: 0.0, prev: [0.0; 2] }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        let step = self.from as f64 / self.to as f64;
        let frames = input.len() / 2;
        // Frame i of the chunk is index i + 1, `prev` is index 0.
        let at = |i: usize| if i == 0 { self.prev } else { [input[(i - 1) * 2], input[(i - 1) * 2 + 1]] };
        while self.pos < frames as f64 {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            self.pos += step;
        }
        if frames > 0 {
            self.prev = at(frames);
            self.pos -= frames as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels() {
        let mut out = Vec::new();
        to_stereo(&[0.5, -0.5], 1, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
        out.clear();
        to_stereo(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3, &mut out);
        assert_eq!(out, [1.0, 2.0, 4.0, 5.0]);
        out.clear();
        from_stereo(&[1.0, 0.0], 1, &mut out);
        assert_eq!(out, [0.5]);
        out.clear();
        from_stereo(&[1.0, 2.0], 4, &mut out);
        assert_eq!(out, [1.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn resampling_keeps_the_length_and_the_shape() {
        // One second of a slow ramp at 44.1 kHz, fed in uneven chunks.
        let input: Vec<f32> = (0..44_100).flat_map(|i| [i as f32 / 44_100.0; 2]).collect();
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for chunk in input.chunks(2 * 441 + 6) {
            r.process(chunk, &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames}");
        // Still a ramp: monotonic, from 0 towards 1.
        assert!(out.chunks(2).zip(out.chunks(2).skip(1)).all(|(a, b)| b[0] >= a[0]));
        assert!(out[out.len() - 2] > 0.99);
        // Same rate: untouched.
        let mut same = Vec::new();
        Resampler::new(48_000, 48_000).process(&[0.1, 0.2], &mut same);
        assert_eq!(same, [0.1, 0.2]);
    }
}
