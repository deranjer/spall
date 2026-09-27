use crate::counters::Timer;
use std::fmt::{Display, Formatter, Result};

/// Performance counters related to constraints resolution.
#[derive(Default, Clone, Copy)]
pub struct SolverCounters {
    /// Pressure iterations executed in the latest solver substep.
    pub pressure_iterations: usize,
    /// Divergence iterations executed in the latest solver substep.
    pub divergence_iterations: usize,
    /// Mean relative density residual checked by the stopping test.
    pub pressure_error: f32,
    /// Mean divergence residual checked by the stopping test.
    pub divergence_error: f32,
    /// Whether the pressure stopping criterion was met before the iteration limit.
    pub pressure_converged: bool,
    /// Whether the divergence stopping criterion was met before the iteration limit.
    pub divergence_converged: bool,
    /// Time spent for the resolution of non-pressure forces.
    pub non_pressure_resolution_time: Timer,
    /// Time spent for the resolution of pressure forces.
    pub pressure_resolution_time: Timer,
}

impl SolverCounters {
    /// Creates a new counter initialized to zero.
    pub fn new() -> Self {
        SolverCounters {
            pressure_iterations: 0,
            divergence_iterations: 0,
            pressure_error: f32::NAN,
            divergence_error: f32::NAN,
            pressure_converged: false,
            divergence_converged: false,
            non_pressure_resolution_time: Timer::new(),
            pressure_resolution_time: Timer::new(),
        }
    }

    /// Enables all the counters for the solver.
    pub fn enable(&mut self) {
        self.non_pressure_resolution_time.enable();
        self.pressure_resolution_time.enable();
    }

    /// Disables all the counters for the solver.
    pub fn disable(&mut self) {
        self.non_pressure_resolution_time.disable();
        self.pressure_resolution_time.disable();
    }

    /// Resets to zero all the counters for the solver.
    pub fn reset(&mut self) {
        self.pressure_iterations = 0;
        self.divergence_iterations = 0;
        self.pressure_error = f32::NAN;
        self.divergence_error = f32::NAN;
        self.pressure_converged = false;
        self.divergence_converged = false;
        self.non_pressure_resolution_time.reset();
        self.pressure_resolution_time.reset();
    }
}

impl Display for SolverCounters {
    fn fmt(&self, f: &mut Formatter) -> Result {
        writeln!(
            f,
            "Non-pressure resolution time: {}",
            self.non_pressure_resolution_time
        )?;
        writeln!(
            f,
            "Pressure resolution time: {}",
            self.pressure_resolution_time
        )
    }
}
