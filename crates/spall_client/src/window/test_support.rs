use super::*;

pub(super) fn view(
    position_m: [f64; 3],
    velocity_m_s: [f32; 3],
    published_at: Instant,
) -> InteractiveView {
    InteractiveView {
        presentation_correction_total: [0.; 3],
        predicted: spall_physics::CharacterState {
            position_m,
            velocity_m_s,
            grounded: true,
            jump_held_last: false,
            endurance: 1.0,
            exhausted: false,
        },
        server_tick: 0,
        published_at,
        corrections: 0,
        max_correction_m: 0.0,
        idle_corrections: 0,
        max_idle_correction_m: 0.0,
        max_vertical_correction_m: 0.0,
        max_horizontal_correction_m: 0.0,
        unmatched_reconciles: 0,
        max_unmatched_displacement_m: 0.0,
        prediction_steps: 0,
        prediction_elapsed_ms: 0,
        prediction_max_backlog_steps: 0,
        prediction_dropped_steps: 0,
        window_stats: crate::predict::WindowStats::default(),
    }
}
