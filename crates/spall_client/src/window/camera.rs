use super::*;

/// How quickly the displayed camera position catches up to the interpolated
/// prediction — see [`CameraFollow::eye`].
/// Short enough to add well under a frame's worth of lag to genuinely
/// continuous movement (WASD keeps moving the target every frame, so
/// smoothing barely touches it), long enough to turn a `PredictedPlayer`
/// reconciliation correction — measured live at a small, consistent ~0.15 m,
/// happening even while standing still (resting-contact micro-jitter
/// between the client's and server's independently-computed physics, not a
/// bug introduced by this window) — into a brief, barely-visible glide
/// instead of a snap.
pub(super) const CORRECTION_SMOOTHING_TAU_S: f32 = 0.05;

/// Render-follow state for the two independently paced mover/render clocks.
///
/// The old camera extrapolated the newest pose by `velocity * elapsed`. That
/// hid the clocks' beat pattern while movement continued, but necessarily put
/// the camera ahead of the simulation. On the first zero-velocity publication
/// after releasing WASD, its target jumped back to the actual stop position —
/// the small backward jerk observed in UAT. Interpolating from the previous
/// publication to the newest one is one sample later, but never invents travel
/// beyond a position the predictor actually reached.
#[derive(Default)]
pub(super) struct CameraFollow {
    correction_presented: Option<[f64; 3]>,
    previous: Option<InteractiveView>,
    current: Option<InteractiveView>,
    displayed: Option<([f64; 3], Instant)>,
}

impl CameraFollow {
    fn target(&mut self, view: InteractiveView, now: Instant) -> [f64; 3] {
        if self
            .current
            .is_none_or(|current| current.published_at != view.published_at)
        {
            self.previous = self.current;
            self.current = Some(view);
        }

        let current = self.current.expect("just installed above");
        let Some(previous) = self.previous else {
            return current.predicted.position_m;
        };
        let sample_span = current
            .published_at
            .saturating_duration_since(previous.published_at)
            .as_secs_f64();
        if sample_span <= f64::EPSILON {
            return current.predicted.position_m;
        }
        let elapsed = now
            .saturating_duration_since(current.published_at)
            .as_secs_f64();
        let t = (elapsed / sample_span).clamp(0.0, 1.0);
        std::array::from_fn(|axis| {
            previous.predicted.position_m[axis]
                + (current.predicted.position_m[axis] - previous.predicted.position_m[axis]) * t
        })
    }

    /// `smooth` applies [`CORRECTION_SMOOTHING_TAU_S`]; it is off for
    /// client-authoritative sessions, which have no corrections to hide and
    /// would otherwise show the camera lagging the bodies it pushes.
    pub(super) fn eye(&mut self, view: InteractiveView, now: Instant, smooth: bool) -> Vec3 {
        let mut target = self.target(view, now);
        // Interpolate ordinary movement independently from reconciliation.
        // A correction should catch up smoothly instead of reversing a moving
        // camera for one frame. Large teleports still apply immediately.
        let total = view.presentation_correction_total;
        let prior = self.correction_presented.unwrap_or(total);
        let distance = (0..3)
            .map(|i| (prior[i] - total[i]).powi(2))
            .sum::<f64>()
            .sqrt();
        let dt = self.displayed.map_or(0., |(_, at)| {
            now.saturating_duration_since(at).as_secs_f64()
        });
        let factor = if !smooth || distance > 1. {
            1.
        } else {
            1. - (-dt / 0.12).exp()
        };
        let presented = std::array::from_fn(|i| prior[i] + (total[i] - prior[i]) * factor);
        self.correction_presented = Some(presented);
        // target() interpolates corrected positions with the same timestamps.
        let interpolated_total = match (self.previous, self.current) {
            (Some(a), Some(b)) => {
                let span = b
                    .published_at
                    .saturating_duration_since(a.published_at)
                    .as_secs_f64();
                let t = if span > 0. {
                    (now.saturating_duration_since(b.published_at).as_secs_f64() / span)
                        .clamp(0., 1.)
                } else {
                    1.
                };
                std::array::from_fn(|i| {
                    a.presentation_correction_total[i]
                        + (b.presentation_correction_total[i] - a.presentation_correction_total[i])
                            * t
                })
            }
            _ => total,
        };
        for i in 0..3 {
            target[i] += presented[i] - interpolated_total[i];
        }
        let feet = match self.displayed {
            Some((prev, prev_at)) if smooth => {
                let dt = now.saturating_duration_since(prev_at).as_secs_f32();
                let factor = 1.0 - (-dt / CORRECTION_SMOOTHING_TAU_S).exp();
                std::array::from_fn(|axis| {
                    prev[axis] + (target[axis] - prev[axis]) * f64::from(factor)
                })
            }
            _ => target,
        };
        self.displayed = Some((feet, now));
        let eye_height_m = f64::from(CharacterParams::DEFAULT.total_height_m()) * 0.9;
        Vec3::new(
            feet[0] as f32,
            (feet[1] + eye_height_m) as f32,
            feet[2] as f32,
        )
    }
}

/// Local wish-independent world-space look direction from yaw/pitch — a free
/// function (not an `InteractiveApp` method) so it can be called from the
/// `RedrawRequested` handler's camera step without borrowing all of `self`
/// while `self.renderer` is already mutably borrowed there — see ENG-69
/// round 13.
pub(super) fn view_dir_from(yaw: f32, pitch: f32) -> [f32; 3] {
    let (sin_y, cos_y) = yaw.sin_cos();
    let (sin_p, cos_p) = pitch.sin_cos();
    [sin_y * cos_p, sin_p, -cos_y * cos_p]
}

/// Vertical field of view of the game camera.
pub(super) const CAMERA_FOV_Y_DEG: f32 = 75.0;
pub(super) const CAMERA_Z_NEAR_M: f32 = 0.05;
pub(super) const CAMERA_Z_FAR_M: f32 = 300.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::test_support::view;

    #[test]
    fn camera_interpolation_does_not_overshoot_and_jerk_back_at_a_stop() {
        let start = Instant::now();
        let tick = Duration::from_millis(16);
        let mut follow = CameraFollow::default();
        let samples = [
            view([0.0, 0.0, 0.0], [4.5, 0.0, 0.0], start),
            view([0.072, 0.0, 0.0], [4.5, 0.0, 0.0], start + tick),
            view([0.144, 0.0, 0.0], [0.0; 3], start + tick * 2),
        ];
        let mut last = f64::NEG_INFINITY;
        for (index, sample) in samples.into_iter().enumerate() {
            for frame in 0..2 {
                let now = start + tick * index as u32 + Duration::from_millis(frame * 8);
                let x = follow.target(sample, now)[0];
                assert!(x >= last, "camera target moved backward: {last} -> {x}");
                assert!(x <= sample.predicted.position_m[0] + f64::EPSILON);
                last = x;
            }
        }
    }

    #[test]
    fn applied_corrections_do_not_reverse_forward_camera_motion_and_settle() {
        let start = Instant::now();
        let tick = Duration::from_millis(16);
        let mut follow = CameraFollow::default();
        let mut last = f32::NEG_INFINITY;
        for n in 0..60 {
            let correction = if n >= 10 { -0.23 } else { 0. };
            let mut sample = view(
                [n as f64 * 0.0768 + correction, 0., 0.],
                [4.8, 0., 0.],
                start + tick * n,
            );
            sample.presentation_correction_total = [correction, 0., 0.];
            let eye = follow.eye(sample, start + tick * n, true);
            assert!(
                eye.x >= last,
                "camera reversed at tick {n}: {last} -> {}",
                eye.x
            );
            last = eye.x;
        }
        let final_x = 59. * 0.0768 - 0.23;
        for n in 60..150 {
            let mut sample = view([final_x, 0., 0.], [0.; 3], start + tick * n);
            sample.presentation_correction_total = [-0.23, 0., 0.];
            last = follow.eye(sample, start + tick * n, true).x;
        }
        assert!((f64::from(last) - final_x).abs() < 0.001);
    }
}
