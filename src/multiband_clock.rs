//! Pure playback timing and zero-based band-window arithmetic.

use std::ops::Range;
use std::time::{Duration, Instant};

pub(super) const DEFAULT_WINDOW: usize = 20;
// A suspended render loop is a discontinuity, not a backlog to replay on resume.
const MAX_FRAME_GAP: Duration = Duration::from_millis(250);
const BAND_FADE: Duration = Duration::from_millis(180);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Endpoints {
    pub current: usize,
    pub next: usize,
    pub fraction: f32,
}

/// Manual band selections crossfade directly, without traversing intervening bands.
pub(super) struct BandFade {
    pub from: Endpoints,
    pub to: usize,
    last_frame: Option<Instant>,
    elapsed: Duration,
}

impl BandFade {
    pub fn new(from: usize, to: usize) -> Self {
        Self::from_frame(
            Endpoints {
                current: from,
                next: from,
                fraction: 0.0,
            },
            to,
        )
    }

    pub fn from_frame(from: Endpoints, to: usize) -> Self {
        Self {
            from,
            to,
            last_frame: None,
            elapsed: Duration::ZERO,
        }
    }

    pub fn retarget(&mut self, to: usize) {
        // Keep an in-progress blend intact; the latest selection follows it.
        if self.last_frame.is_none() {
            self.to = to;
        }
    }

    pub fn frame(&mut self, now: Instant, ready: bool) -> Endpoints {
        if ready {
            if let Some(last) = self.last_frame.replace(now) {
                // Newly cached windows still need GPU upload/submission. Do not
                // let one slow frame consume the whole fade without a blend.
                self.elapsed += now.saturating_duration_since(last).min(BAND_FADE / 2);
            }
        } else if self.last_frame.is_some() {
            self.last_frame = Some(now);
        }
        let fraction = (self.elapsed.as_secs_f32() / BAND_FADE.as_secs_f32()).min(1.0);
        Endpoints {
            current: self.from.current,
            next: self.to,
            fraction,
        }
    }
}

pub(super) fn endpoints(position: f64, count: usize, interpolate: bool) -> Endpoints {
    let position = normalize(position, count);
    let current = position.floor() as usize;
    let fraction = if interpolate { position.fract() as f32 } else { 0.0 };
    let next = if fraction > 0.0 { (current + 1) % count.max(1) } else { current };
    Endpoints { current, next, fraction }
}

fn normalize(position: f64, count: usize) -> f64 {
    if position.is_finite() && count > 0 {
        position.rem_euclid(count as f64)
    } else {
        0.0
    }
}

pub(super) fn window_start(band: usize, size: usize) -> usize {
    band / size * size
}

pub(super) fn next_window(start: usize, size: usize, count: usize) -> usize {
    let next = start + size;
    if next < count { next } else { 0 }
}

pub(super) fn window_range(start: usize, size: usize, count: usize) -> Range<usize> {
    start..(start + size).min(count)
}

pub(super) struct PlaybackClock {
    position: f64,
    pending: Option<f64>,
    remaining_bands: f64,
    last: Instant,
    buffering: bool,
}

impl PlaybackClock {
    pub fn new(now: Instant) -> Self {
        Self {
            position: 0.0,
            pending: None,
            remaining_bands: 0.0,
            last: now,
            buffering: true,
        }
    }

    pub fn seek(&mut self, position: f64, count: usize, now: Instant) {
        self.position = normalize(position, count);
        self.pending = None;
        self.remaining_bands = 0.0;
        self.last = now;
        self.buffering = true;
    }

    pub fn position(&self) -> f64 {
        self.position
    }

    pub fn is_buffering(&self) -> bool {
        self.buffering
    }

    /// Advance at most one band per readiness check, retaining any unspent time
    /// across integer boundaries. Buffering and render-loop discontinuities drop
    /// this credit, so resuming never replays time spent waiting for data.
    pub fn propose(&mut self, now: Instant, speed: f64, count: usize, playing: bool) -> f64 {
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;
        if let Some(pending) = self.pending {
            return pending;
        }
        if self.buffering || !playing || !speed.is_finite() || speed <= 0.0 {
            self.remaining_bands = 0.0;
            return self.position;
        }
        if elapsed > MAX_FRAME_GAP {
            self.remaining_bands = 0.0;
            return self.position;
        }
        self.remaining_bands += elapsed.as_secs_f64() * speed;
        let step = self.remaining_bands.min(1.0);
        self.remaining_bands -= step;
        normalize(self.position + step, count)
    }

    pub fn resolve(&mut self, proposed: f64, ready: bool) {
        self.buffering = !ready;
        if !ready {
            self.remaining_bands = 0.0;
        }
        if ready {
            self.position = proposed;
            self.pending = None;
        } else if proposed != self.position {
            self.pending = Some(proposed);
        }
    }

    pub fn freeze(&mut self, now: Instant) {
        self.remaining_bands = 0.0;
        self.last = now;
        self.buffering = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_band_fade_waits_for_data_then_blends_directly_to_the_selected_band() {
        let now = Instant::now();
        let mut fade = BandFade::new(2, 42);
        assert_eq!(
            fade.frame(now, false),
            Endpoints {
                current: 2,
                next: 42,
                fraction: 0.0
            }
        );
        let start = now + Duration::from_secs(30);
        assert_eq!(fade.frame(start, true).fraction, 0.0);
        assert_eq!(fade.frame(start + BAND_FADE / 2, true).fraction, 0.5);
        assert_eq!(fade.frame(start + BAND_FADE * 2, true).fraction, 1.0);
    }

    #[test]
    fn manual_band_fade_retargets_loading_but_finishes_an_active_blend() {
        let now = Instant::now();
        let mut fade = BandFade::new(2, 3);
        fade.retarget(42);
        assert_eq!(
            fade.frame(now, true),
            Endpoints {
                current: 2,
                next: 42,
                fraction: 0.0
            }
        );
        fade.retarget(7);
        assert_eq!(
            fade.frame(now + BAND_FADE / 2, true),
            Endpoints {
                current: 2,
                next: 42,
                fraction: 0.5
            }
        );
    }

    #[test]
    fn manual_band_fade_keeps_a_blended_frame_after_a_slow_upload() {
        let now = Instant::now();
        let mut fade = BandFade::new(0, 42);
        assert_eq!(fade.frame(now, true).fraction, 0.0);
        assert_eq!(fade.frame(now + Duration::from_secs(2), true).fraction, 0.5);
        assert_eq!(fade.frame(now + Duration::from_secs(4), true).fraction, 1.0);
    }

    #[test]
    fn manual_band_fade_does_not_advance_while_replacement_coverage_loads() {
        let now = Instant::now();
        let mut fade = BandFade::new(0, 42);
        fade.frame(now, true);
        assert_eq!(fade.frame(now + Duration::from_millis(45), true).fraction, 0.25);
        assert_eq!(fade.frame(now + Duration::from_secs(10), false).fraction, 0.25);
        assert_eq!(fade.frame(now + Duration::from_secs(10), true).fraction, 0.25);
    }

    #[test]
    fn final_band_interpolates_back_to_first() {
        assert_eq!(
            endpoints(42.5, 43, true),
            Endpoints {
                current: 42,
                next: 0,
                fraction: 0.5
            }
        );
    }

    #[test]
    fn reduced_motion_keeps_explicit_fractional_position_but_steps_rendering() {
        assert_eq!(
            endpoints(19.75, 43, false),
            Endpoints {
                current: 19,
                next: 19,
                fraction: 0.0
            }
        );
    }

    #[test]
    fn partial_final_window_wraps_prefetch_to_zero() {
        assert_eq!((window_range(40, 20, 43), next_window(40, 20, 43)), (40..43, 0));
    }

    #[test]
    fn boundary_requires_both_windows_only_when_interpolating() {
        let frame = endpoints(19.5, 43, true);
        assert_eq!((window_start(frame.current, DEFAULT_WINDOW), window_start(frame.next, DEFAULT_WINDOW)), (0, 20));
    }

    #[test]
    fn empty_and_nonfinite_positions_are_safe() {
        assert_eq!(
            endpoints(f64::NAN, 0, true),
            Endpoints {
                current: 0,
                next: 0,
                fraction: 0.0
            }
        );
    }

    #[test]
    fn initial_buffering_does_not_advance_clock() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        assert_eq!(clock.propose(now + Duration::from_secs(10), 10.0, 43, true), 0.0);
    }

    #[test]
    fn missing_visible_tile_freezes_every_tile_at_accepted_position() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        clock.seek(19.0, 43, now);
        clock.resolve(19.0, true);
        let proposed = clock.propose(now + Duration::from_millis(50), 10.0, 43, true);
        clock.resolve(proposed, false);
        assert_eq!((clock.position(), clock.is_buffering()), (19.0, true));
    }

    #[test]
    fn buffering_resumes_at_pending_frame_without_catching_up() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        clock.resolve(19.0, true);
        clock.resolve(19.5, false);
        let proposed = clock.propose(now + Duration::from_secs(60), 10.0, 43, true);
        clock.resolve(proposed, true);
        assert_eq!(clock.propose(now + Duration::from_millis(60016), 10.0, 43, true), 19.66);
    }

    #[test]
    fn delayed_paint_advances_at_most_one_band_instead_of_skipping_windows() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        clock.resolve(19.5, true);
        assert_eq!(clock.propose(now + Duration::from_millis(100), 60.0, 43, true), 20.5);
    }

    #[test]
    fn long_render_gap_discards_catchup_as_a_discontinuity() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        clock.resolve(19.5, true);
        let proposed = clock.propose(now + Duration::from_secs(60), 60.0, 43, true);
        clock.resolve(proposed, true);
        assert_eq!(proposed, 19.5);
        assert!((clock.propose(now + Duration::from_millis(60016), 60.0, 43, true) - 20.46).abs() < 1e-10);
    }

    #[test]
    fn hundred_cached_frames_preserve_requested_ten_and_sixty_band_rates() {
        for speed in [10.0, 60.0] {
            let now = Instant::now();
            let mut clock = PlaybackClock::new(now);
            clock.resolve(0.0, true);
            for tick in 1..=100 {
                let proposed = clock.propose(now + Duration::from_millis(tick * 16), speed, 256, true);
                clock.resolve(proposed, true);
            }
            let expected = 1.6 * speed;
            assert!(
                (clock.position() - expected).abs() < 1e-10,
                "Speed {speed}: got {}, expected {expected}",
                clock.position()
            );
        }
    }

    #[test]
    fn cached_delayed_frames_retain_unspent_elapsed_time() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        clock.resolve(0.0, true);
        for _ in 0..6 {
            let proposed = clock.propose(now + Duration::from_millis(100), 60.0, 43, true);
            clock.resolve(proposed, true);
        }
        assert_eq!(clock.position(), 6.0);
    }

    #[test]
    fn buffering_discards_unspent_credit_and_does_not_catch_up() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        clock.resolve(0.0, true);
        let pending = clock.propose(now + Duration::from_millis(100), 60.0, 43, true);
        clock.resolve(pending, false);
        let resumed = clock.propose(now + Duration::from_secs(60), 60.0, 43, true);
        clock.resolve(resumed, true);
        let next = clock.propose(now + Duration::from_millis(60016), 60.0, 43, true);
        assert!((next - 1.96).abs() < 1e-10);
    }

    #[test]
    fn coverage_change_discards_elapsed_time_even_when_new_tiles_are_cached() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        clock.resolve(4.25, true);
        clock.freeze(now + Duration::from_secs(60));
        assert_eq!(clock.propose(now + Duration::from_secs(60), 10.0, 43, true), 4.25);
    }

    #[test]
    fn paused_seek_is_immediate_and_stays_put() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        clock.seek(31.5, 43, now);
        clock.resolve(31.5, true);
        assert_eq!(clock.propose(now + Duration::from_secs(10), 10.0, 43, false), 31.5);
    }

    #[test]
    fn accepted_last_band_wraps_to_first() {
        let now = Instant::now();
        let mut clock = PlaybackClock::new(now);
        clock.resolve(42.5, true);
        assert_eq!(clock.propose(now + Duration::from_millis(50), 10.0, 43, true), 0.0);
    }
}
