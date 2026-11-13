//! The angle of a card hanging from its clothespin.
//!
//! A card is not a rigid iron block on a hinge: it has a card-sized moment of
//! inertia, and its motion loses energy to air drag that grows with the square
//! of the angular speed. This integrator owns that angle; the scene feeds it
//! the pin's horizontal and vertical accelerations and reads its velocity for
//! the bottom-edge shear.
//!
//! The equation is a forced, damped pendulum in the small-angle form used by
//! the rest of the scene (degrees, not radians):
//!
//! ```text
//! θ'' = ω² (target − θ) − 2 ζ ω θ' − drag · |θ'| · θ'
//! ```
//!
//! `ω = sqrt(g_eff / L)` comes from the card's effective length and the
//! effective gravity at the pin, so a portrait photo and a wide screenshot do
//! not share one period.

/// Linear damping ratio. A paper card loses most of a swing in one overshoot;
/// the old 0.30 left two or three visible oscillations, like a heavy hinge.
#[cfg(test)]
const DAMPING: f32 = 0.45;
/// Quadratic air-drag coefficient, 1/degree. The term is in degrees/s², so a
/// fast swing is braked hard while a slow one is left to the linear damping.
#[cfg(test)]
const DRAG: f32 = 0.05;
/// The explicit integrator is stable at 240 Hz for the natural frequencies of
/// a 60–220 DIP card. Substepping also keeps the result independent of the
/// display's refresh rate.
const SUBSTEPS: f32 = 240.0;
/// The angle is settled when it is this close to its target, in degrees.
const SETTLE_ANGLE: f32 = 0.01;
/// The angular speed is settled below this, in degrees/second.
const SETTLE_SPEED: f32 = 0.05;

/// A card's pendulum angle in degrees. Positive is clockwise in Direct2D, so
/// the bottom of the card moves left.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Pendulum {
    pub angle: f32,
    pub velocity: f32,
    pub target: f32,
    omega: f32,
    damping: f32,
    drag: f32,
}

impl Pendulum {
    #[cfg(test)]
    pub(crate) fn new(omega: f32) -> Self {
        Self::with_damping(omega, DAMPING, DRAG)
    }

    /// A card with its own paper coefficients: damping and quadratic drag grow
    /// with the card's aspect ratio, so a tall strip loses a swing faster than
    /// a wide screenshot.
    pub(crate) fn new_with(omega: f32, damping: f32, drag: f32) -> Self {
        Self::with_damping(omega, damping, drag)
    }

    /// The tests use this to compare a dragless pendulum with a paper one.
    fn with_damping(omega: f32, damping: f32, drag: f32) -> Self {
        Self {
            angle: 0.0,
            velocity: 0.0,
            target: 0.0,
            omega: omega.max(1.0),
            damping,
            drag,
        }
    }

    /// Effective gravity and card length change with the pin's vertical
    /// acceleration; the scene refreshes the natural frequency every frame.
    pub(crate) fn set_omega(&mut self, omega: f32) {
        self.omega = omega.max(1.0);
    }

    #[cfg(test)]
    pub(crate) fn step(&mut self, dt: f32) {
        self.step_with(dt, 0.0);
    }

    /// Advance by `dt`, adding an external angular acceleration. The scene
    /// feeds the relative-wind torque here; it is a force, not a target, so a
    /// card moving through still air keeps a small backward tilt instead of
    /// snapping upright.
    pub(crate) fn step_with(&mut self, dt: f32, wind: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let wind = if wind.is_finite() { wind } else { 0.0 };
        // A frame can be 16.7 ms or 8.3 ms; substep both to the same 240 Hz
        // grid so the damping does not change with the display.
        let steps = (dt * SUBSTEPS).ceil().max(1.0) as usize;
        let h = dt / steps as f32;
        for _ in 0..steps {
            let restoring = self.omega * self.omega * (self.target - self.angle);
            let linear = 2.0 * self.damping * self.omega * self.velocity;
            let quadratic = self.drag * self.velocity.abs() * self.velocity;
            self.velocity += (restoring - linear - quadratic + wind) * h;
            self.angle += self.velocity * h;
        }
    }

    /// Put the angle at rest at `angle` (and make that the target). Used when
    /// a grab starts, or when the flight window takes the card over.
    pub(crate) fn snap(&mut self, angle: f32) {
        self.angle = angle;
        self.target = angle;
        self.velocity = 0.0;
    }

    pub(crate) fn settled(&self) -> bool {
        (self.angle - self.target).abs() < SETTLE_ANGLE && self.velocity.abs() < SETTLE_SPEED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(pendulum: &mut Pendulum, seconds: f32) -> f32 {
        let dt = 1.0 / 60.0;
        let mut peak = pendulum.angle.abs();
        let frames = (seconds / dt).round() as usize;
        for _ in 0..frames {
            pendulum.step(dt);
            peak = peak.max(pendulum.angle.abs());
        }
        peak
    }

    #[test]
    fn a_paper_pendulum_overshoots_once_and_settles() {
        let mut pendulum = Pendulum::new(18.0);
        pendulum.target = 6.0;
        let peak = run(&mut pendulum, 2.0);
        assert!(peak < 8.0, "the swing stays near the target, peak={peak}");
        assert!(
            pendulum.settled(),
            "the paper pendulum settles, angle={} velocity={}",
            pendulum.angle,
            pendulum.velocity
        );
    }

    #[test]
    fn quadratic_drag_eats_a_large_swing() {
        let mut paper = Pendulum::with_damping(18.0, 0.0, DRAG);
        let mut free = Pendulum::with_damping(18.0, 0.0, 0.0);
        paper.angle = 12.0;
        free.angle = 12.0;
        // Both start with the same energy and no restoring target, so only
        // the drag terms can slow them down. After half a second the paper
        // card is closer to rest; comparing peaks would only measure the
        // initial angle, which is the same for both.
        paper.target = 0.0;
        free.target = 0.0;
        let dt = 1.0 / 60.0;
        for _ in 0..30 {
            paper.step(dt);
            free.step(dt);
        }
        assert!(
            paper.angle.abs() < free.angle.abs(),
            "the quadratic drag removes more speed: paper={} free={}",
            paper.angle,
            free.angle
        );
    }

    #[test]
    fn a_relative_wind_torque_tips_the_card() {
        let mut rightward = Pendulum::new(18.0);
        let mut leftward = Pendulum::new(18.0);
        rightward.step_with(0.1, 200.0);
        leftward.step_with(0.1, -200.0);
        assert!(
            rightward.angle > 0.0,
            "rightward motion tips the bottom left, angle={}",
            rightward.angle
        );
        assert!(
            leftward.angle < 0.0,
            "leftward motion tips the bottom right, angle={}",
            leftward.angle
        );
    }

    #[test]
    fn a_stiffer_pendulum_crosses_sooner() {
        let mut slow = Pendulum::new(12.0);
        let mut quick = Pendulum::new(24.0);
        slow.target = 6.0;
        quick.target = 6.0;
        slow.step(0.1);
        quick.step(0.1);
        assert!(
            quick.angle > slow.angle,
            "the shorter card swings faster: quick={} slow={}",
            quick.angle,
            slow.angle
        );
    }
}
