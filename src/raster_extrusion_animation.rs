//! Presentation-only height transitions; raw raster data and playback stay unchanged.

use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(crate) struct ExtrusionAnimation {
    transition: Option<Transition>,
    height: Option<f32>,
}

struct Transition {
    from: f32,
    to: f32,
    started: Instant,
}

impl Transition {
    fn sample(&self, now: Instant) -> f32 {
        let progress = (now.duration_since(self.started).as_secs_f32() / DURATION.as_secs_f32()).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        self.from + (self.to - self.from) * eased
    }
}

impl ExtrusionAnimation {
    pub(crate) fn retarget(&mut self, previous: (bool, f32), target: (bool, f32), now: Instant) {
        if previous == target {
            return;
        }
        if previous.0 == target.0 && !self.is_active() {
            return;
        }
        let from = self
            .transition
            .as_ref()
            .map_or(if previous.0 { previous.1 } else { 0.0 }, |transition| transition.sample(now));
        let to = if target.0 { target.1 } else { 0.0 };
        self.transition = Some(Transition { from, to, started: now });
        self.height = Some(from);
    }

    pub(crate) fn advance(&mut self, now: Instant) {
        let Some(transition) = &self.transition else { return };
        if now.duration_since(transition.started) >= DURATION {
            *self = Self::default();
        } else {
            self.height = Some(transition.sample(now));
        }
    }

    pub(crate) fn height(&self, target: f32) -> f32 {
        self.height.unwrap_or(target)
    }

    pub(crate) fn maximum_height(&self, target: f32) -> f32 {
        self.transition.as_ref().map_or(target, |transition| transition.from.max(transition.to))
    }

    pub(crate) fn is_active(&self) -> bool {
        self.transition.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrusion_toggle_grows_from_ground_and_collapses_to_ground() {
        let now = Instant::now();
        let mut animation = ExtrusionAnimation::default();
        animation.retarget((false, 4.0), (true, 4.0), now);
        assert_eq!(animation.height(4.0), 0.0);
        animation.advance(now + DURATION / 2);
        assert_eq!(animation.height(4.0), 3.5);
        animation.advance(now + DURATION);
        assert_eq!(animation.height(4.0), 4.0);
        assert!(!animation.is_active());

        animation.retarget((true, 4.0), (false, 4.0), now + DURATION);
        assert_eq!(animation.height(0.0), 4.0);
        assert_eq!(animation.maximum_height(0.0), 4.0);
        animation.advance(now + DURATION * 3 / 2);
        assert_eq!(animation.height(0.0), 0.5);
        animation.advance(now + DURATION * 2);
        assert_eq!(animation.height(0.0), 0.0);
        assert!(!animation.is_active());
    }

    #[test]
    fn extrusion_toggle_reverses_from_current_height_without_restarting_on_style_updates() {
        let now = Instant::now();
        let mut animation = ExtrusionAnimation::default();
        animation.retarget((false, 4.0), (true, 4.0), now);
        animation.retarget((true, 4.0), (true, 4.0), now + DURATION / 4);
        animation.retarget((true, 4.0), (false, 4.0), now + DURATION / 2);
        assert_eq!(animation.height(0.0), 3.5);
        animation.advance(now + DURATION);
        assert_eq!(animation.height(0.0), 0.4375);
        animation.retarget((false, 4.0), (true, 4.0), now + DURATION);
        assert_eq!(animation.height(4.0), 0.4375);
        animation.advance(now + DURATION * 2);
        assert_eq!(animation.height(4.0), 4.0);
    }

    #[test]
    fn extrusion_height_slider_is_immediate_unless_a_toggle_is_animating() {
        let now = Instant::now();
        let mut animation = ExtrusionAnimation::default();
        animation.retarget((true, 2.0), (true, 4.0), now);
        assert!(!animation.is_active());
        assert_eq!(animation.height(4.0), 4.0);
        animation.retarget((true, 4.0), (false, 4.0), now);
        animation.retarget((false, 4.0), (true, 8.0), now + DURATION / 2);
        animation.advance(now + DURATION);
        assert_eq!(animation.height(8.0), 7.0625);
    }

    #[test]
    fn extrusion_ease_out_slows_toward_the_endpoint_in_both_directions() {
        let now = Instant::now();
        for (from, to) in [(0.0, 8.0), (8.0, 0.0)] {
            let transition = Transition { from, to, started: now };
            let heights = [0, 1, 2, 3, 4].map(|quarter| transition.sample(now + DURATION * quarter / 4));
            let distances: Vec<_> = heights.windows(2).map(|pair| (pair[1] - pair[0]).abs()).collect();
            assert_eq!(heights[0], from);
            assert_eq!(heights[4], to);
            assert!(
                distances.windows(2).all(|pair| pair[0] > pair[1]),
                "Each equal time interval must travel less distance"
            );
            assert!(distances[3] > 0.0, "The animation must not finish early");
        }
    }
}
