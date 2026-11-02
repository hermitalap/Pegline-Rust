//! The damped spring every moving value owns, ported from `Support.cs`.
//!
//! The integration is semi-implicit Euler with 1/240 s sub-steps, exactly like
//! the WPF version, so a spring given the same response/damping and the same
//! elapsed time lands in the same place at 60 Hz and 120 Hz.

/// A one-dimensional damped spring with a fixed target.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    stiffness: f32,
    damping: f32,
}

impl Spring {
    /// SwiftUI's `spring(response:dampingFraction:)`.
    pub fn new(response: f32, damping_ratio: f32, value: f32) -> Self {
        let mut spring = Self {
            value,
            velocity: 0.0,
            target: value,
            stiffness: 0.0,
            damping: 0.0,
        };
        spring.set_response(response, damping_ratio);
        spring
    }

    /// Rescales the coefficients; the current value and velocity are kept.
    pub fn set_response(&mut self, response: f32, damping_ratio: f32) {
        let response = response.max(0.001);
        self.stiffness = (std::f32::consts::TAU / response).powi(2);
        self.damping = 4.0 * std::f32::consts::PI * damping_ratio / response;
    }

    /// SwiftUI's interpolatingSpring, unit mass.
    pub fn set_stiffness(&mut self, stiffness: f32, damping: f32) {
        self.stiffness = stiffness;
        self.damping = damping;
    }

    /// The largest distance and speed that are still "at rest". At 6K a card
    /// sits around x = 1600 DIP, where one f32 ulp is ~0.0001 DIP; a tighter
    /// threshold than this makes the spring creep forever below the precision
    /// of its own coordinates (the card never settles and the frame loop never
    /// idles).
    const REST_DISTANCE: f32 = 0.01;
    /// See [`Self::REST_DISTANCE`]; degrees/DIP per second are invisible here.
    const REST_SPEED: f32 = 0.05;

    /// Advances by `dt` seconds, split into steps of at most 1/240 s.
    pub fn step(&mut self, dt: f32) {
        if dt <= 0.0 {
            return;
        }
        if (self.value - self.target).abs() < Self::REST_DISTANCE
            && self.velocity.abs() < Self::REST_SPEED
        {
            // Snap out of the asymptotic tail instead of creeping below the
            // representable step of a large coordinate.
            self.value = self.target;
            self.velocity = 0.0;
            return;
        }
        let steps = ((dt * 240.0).ceil() as usize).max(1);
        let h = dt / steps as f32;
        for _ in 0..steps {
            let acceleration =
                -self.stiffness * (self.value - self.target) - self.damping * self.velocity;
            self.velocity += acceleration * h;
            self.value += self.velocity * h;
        }
    }

    /// Close enough that the next frame would not visibly move.
    pub fn settled(&self) -> bool {
        (self.value - self.target).abs() < Self::REST_DISTANCE
            && self.velocity.abs() < Self::REST_SPEED
    }

    /// Snaps to the target and stops. Used when a card is created already in
    /// place, and when the strip is hidden (velocity must not survive a hide).
    pub fn snap(&mut self, value: f32) {
        self.value = value;
        self.target = value;
        self.velocity = 0.0;
    }
}

/// Easing helpers ported from `Ease` in `Support.cs`.
pub mod ease {
    pub fn in_out_cubic(x: f32) -> f32 {
        if x < 0.5 {
            4.0 * x * x * x
        } else {
            let t = -2.0 * x + 2.0;
            1.0 - t * t * t / 2.0
        }
    }

    pub fn smooth(x: f32, a: f32, b: f32) -> f32 {
        let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }

    pub fn out_quad(x: f32) -> f32 {
        1.0 - (1.0 - x) * (1.0 - x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_to_target() {
        let mut spring = Spring::new(0.42, 0.82, 0.0);
        spring.target = 100.0;
        for _ in 0..60 {
            spring.step(1.0 / 60.0);
        }
        assert!((spring.value - 100.0).abs() < 0.5, "value={}", spring.value);
    }

    #[test]
    fn frame_rate_independent_after_one_second() {
        let mut fast = Spring::new(0.55, 0.78, 0.0);
        let mut slow = Spring::new(0.55, 0.78, 0.0);
        fast.target = 174.0;
        slow.target = 174.0;
        for _ in 0..120 {
            fast.step(1.0 / 120.0);
        }
        for _ in 0..60 {
            slow.step(1.0 / 60.0);
        }
        assert!(
            (fast.value - slow.value).abs() < 0.05,
            "120Hz={} 60Hz={}",
            fast.value,
            slow.value
        );
    }

    #[test]
    fn settled_reports_when_at_rest() {
        let spring = Spring::new(0.4, 0.8, 5.0);
        assert!(spring.settled());
        let mut moving = Spring::new(0.4, 0.8, 5.0);
        moving.target = 10.0;
        moving.step(1.0 / 60.0);
        assert!(!moving.settled());
    }

    #[test]
    fn snap_clears_velocity() {
        let mut spring = Spring::new(0.4, 0.8, 0.0);
        spring.target = 10.0;
        spring.step(0.1);
        spring.snap(-222.0);
        assert_eq!(spring.value, -222.0);
        assert_eq!(spring.velocity, 0.0);
        assert!(spring.settled());
    }

    #[test]
    fn does_not_creep_below_float_precision() {
        // A card slot at 6K widths lives around x = 1600 DIP; without a snap
        // the last few thousandths of a DIP are below one f32 ulp and the
        // spring keeps a tiny velocity forever.
        let mut spring = Spring::new(0.55, 0.78, 0.0);
        spring.target = 1591.0;
        for _ in 0..600 {
            spring.step(1.0 / 60.0);
        }
        assert!(
            spring.settled(),
            "value={} velocity={}",
            spring.value,
            spring.velocity
        );
        assert_eq!(spring.value, 1591.0);
    }
}
