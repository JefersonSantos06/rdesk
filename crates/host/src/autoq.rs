//! Automatic quality ([`rdesk_proto::ExtClient::AutoQuality`]): the host watches how long
//! frames take to be acknowledged and trades JPEG quality, then picture size, for a link that
//! keeps up (like the adaptive quality of other remote desktops, or WebRTC's delay-based
//! congestion control). The frame rate already falls by itself when the link is slow (at most
//! two frames in flight); quality and size decide how much each frame costs.

use std::time::{Duration, Instant};

/// Lowest JPEG quality automatic quality goes down to.
pub const MIN_QUALITY: u8 = 35;
/// Smallest picture, in percent of the monitor.
pub const MIN_SCALE: u8 = 50;
const QUALITY_STEP: u8 = 10;
const SCALE_STEP: u8 = 25;
/// How often a decision is made.
const EVERY: Duration = Duration::from_millis(500);
/// A link has to keep up this long before quality goes up again.
const CALM_FOR: Duration = Duration::from_secs(3);
/// The picture size changes at most this often (each change resends the whole screen).
const SCALE_EVERY: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    pub jpeg_quality: u8,
    pub scale_pct: u8,
}

pub struct AutoQuality {
    choice: Choice,
    /// Smoothed time from sending a frame to its acknowledgement.
    delivery: Option<Duration>,
    acks: u32,
    last_decision: Instant,
    calm_since: Option<Instant>,
    last_scale: Instant,
}

impl AutoQuality {
    pub fn new(ceiling: u8, now: Instant) -> Self {
        Self {
            choice: Choice { jpeg_quality: ceiling, scale_pct: 100 },
            delivery: None,
            acks: 0,
            last_decision: now,
            calm_since: None,
            last_scale: now - SCALE_EVERY,
        }
    }

    pub fn choice(&self) -> Choice {
        self.choice
    }

    /// A frame was acknowledged `took` after it was sent.
    pub fn acked(&mut self, took: Duration) {
        self.acks += 1;
        self.delivery = Some(match self.delivery {
            None => took,
            Some(d) => d.mul_f64(0.8) + took.mul_f64(0.2),
        });
    }

    /// Decide again (at most every [`EVERY`]); `Some` when the choice changed. `oldest` is the
    /// age of the oldest frame still unacknowledged.
    pub fn decide(
        &mut self,
        now: Instant,
        rtt: Duration,
        interval: Duration,
        ceiling: u8,
        oldest: Option<Duration>,
    ) -> Option<Choice> {
        if now.duration_since(self.last_decision) < EVERY {
            return None;
        }
        self.last_decision = now;
        let before = self.choice;
        self.choice.jpeg_quality = self.choice.jpeg_quality.min(ceiling.max(MIN_QUALITY));
        // Queueing: frames take much longer than the round trip plus the time between frames.
        let budget = rtt + interval.max(Duration::from_millis(40)) + Duration::from_millis(60);
        let slow = self.delivery.is_some_and(|d| d > budget) || oldest.is_some_and(|o| o > budget * 3);
        let idle = self.acks == 0;
        self.acks = 0;
        if slow {
            self.calm_since = None;
            if self.choice.jpeg_quality > MIN_QUALITY {
                self.choice.jpeg_quality = self.choice.jpeg_quality.saturating_sub(QUALITY_STEP).max(MIN_QUALITY);
            } else if self.choice.scale_pct > MIN_SCALE && now.duration_since(self.last_scale) >= SCALE_EVERY {
                self.choice.scale_pct = self.choice.scale_pct.saturating_sub(SCALE_STEP).max(MIN_SCALE);
                self.last_scale = now;
            }
        } else if !idle {
            let calm = *self.calm_since.get_or_insert(now);
            if now.duration_since(calm) >= CALM_FOR {
                self.calm_since = Some(now);
                if self.choice.scale_pct < 100 && now.duration_since(self.last_scale) >= SCALE_EVERY {
                    self.choice.scale_pct = (self.choice.scale_pct + SCALE_STEP).min(100);
                    self.last_scale = now;
                } else if self.choice.scale_pct == 100 && self.choice.jpeg_quality < ceiling {
                    self.choice.jpeg_quality = (self.choice.jpeg_quality + QUALITY_STEP / 2).min(ceiling);
                }
            }
        }
        (self.choice != before).then_some(self.choice)
    }
}

/// `w`×`h` at `pct` percent (at least 64 pixels on a side).
pub fn scaled(w: u32, h: u32, pct: u8) -> (u32, u32) {
    if pct >= 100 {
        return (w, h);
    }
    ((w * pct as u32 / 100).max(64.min(w)), (h * pct as u32 / 100).max(64.min(h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RTT: Duration = Duration::from_millis(50);
    const FRAME: Duration = Duration::from_millis(33);

    fn run(a: &mut AutoQuality, t: &mut Instant, took: Duration, steps: u32) -> Choice {
        for _ in 0..steps {
            *t += EVERY;
            a.acked(took);
            a.decide(*t, RTT, FRAME, 85, None);
        }
        a.choice()
    }

    #[test]
    fn a_slow_link_lowers_quality_then_size_and_a_fast_one_brings_them_back() {
        let mut t = Instant::now();
        let mut a = AutoQuality::new(85, t);
        let c = run(&mut a, &mut t, Duration::from_millis(600), 6);
        assert_eq!(c.jpeg_quality, MIN_QUALITY);
        let c = run(&mut a, &mut t, Duration::from_millis(600), 40);
        assert_eq!(c.scale_pct, MIN_SCALE);
        // The link recovers: size first, then quality, back to the ceiling.
        let c = run(&mut a, &mut t, Duration::from_millis(60), 200);
        assert_eq!(c, Choice { jpeg_quality: 85, scale_pct: 100 });
    }

    #[test]
    fn a_link_that_keeps_up_keeps_the_ceiling() {
        let mut t = Instant::now();
        let mut a = AutoQuality::new(85, t);
        assert_eq!(run(&mut a, &mut t, Duration::from_millis(70), 20), Choice { jpeg_quality: 85, scale_pct: 100 });
    }

    #[test]
    fn a_stuck_frame_counts_as_slow() {
        let mut t = Instant::now();
        let mut a = AutoQuality::new(85, t);
        t += EVERY;
        assert!(a.decide(t, RTT, FRAME, 85, Some(Duration::from_secs(2))).is_some());
        assert_eq!(a.choice().jpeg_quality, 75);
    }

    #[test]
    fn scaled_sizes() {
        assert_eq!(scaled(1920, 1080, 100), (1920, 1080));
        assert_eq!(scaled(1920, 1080, 50), (960, 540));
        assert_eq!(scaled(100, 40, 50), (64, 40));
    }
}
