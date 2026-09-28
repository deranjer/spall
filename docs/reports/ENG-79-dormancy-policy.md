# ENG-79 — dormancy policy evaluation

Status: parameter evaluation complete; no new settle criterion adopted.

The existing in-process `DormancyPolicy` was swept at its policy boundary with
a bounded transition tracker. The trace models the observed single-terrain
collider cadence: a sleeping rubble body is disturbed by a terrain edit every
28 ticks. A terrain edit is represented as a hard wake only when the body is
already dormant, matching `Simulation::apply_dormancy`.

| `settle_ticks` | Deactivations in 112 ticks | Hard reactivations | Final state |
| ---: | ---: | ---: | --- |
| 120 | 0 | 0 | awake |
| 20 | 4 | 4 | awake |

The 120-tick default cannot reach dormancy during a 28-tick quiet interval.
The existing policy needs no new terrain-edit-specific criterion: a shorter
settle window reaches dormancy, and each subsequent nearby terrain edit uses
the existing hard-wake path even with `min_dormant_ticks = 10,000`. A separate
invariant sweep confirms that a moving body (`speed > still_speed_m_s`) is
never deactivated, even when `settle_ticks = 1`.

Checks:

- `cargo test -p spall_sim dormancy::tests::settle_window --lib` — 2 passed.
- `cargo test -p spall_sim --test dormancy` — 6 passed.

This is an in-process policy measurement, not a networked performance gate.
The next step is an alternating networked run using `settle_ticks` around the
measured interval (for example 20) before changing `DormancyConfig::DEFAULT`.
Support-removal, body-edit wake, proximity wake, conservation, and impulse
behavior remain covered by the existing dormancy/physics scenarios.

Recommendations for related tickets: keep ENG-75 open because this policy-only
sweep does not establish the sustained networked tick p95/p99 gate; prioritize
ENG-76 after the alternating run because the awake-rubble population is its
direct cost driver; keep ENG-80 experimental/default-off until the dormancy
churn and residency requirements are measured again under the selected policy.
