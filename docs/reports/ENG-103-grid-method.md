# ENG-103 bounded MAC/VOF grid experiment

Status: method selected and documented before implementation. This is a
bounded CPU comparison backend; it is not integrated into the authoritative
simulation tick.

## Published method basis

The velocity/pressure discretization follows the Marker-and-Cell (MAC)
staggered-grid arrangement introduced by Harlow and Welch: pressure and liquid
fraction are cell-centered; each normal velocity component is stored once on
the corresponding cell face. The incompressible velocity update uses a
pressure projection on that grid. The free-surface liquid volume is tracked
with a conservative flux-corrected transport (FCT) volume-tracking method in
the family described by Rudman, building on fractional Volume of Fluid (VOF)
tracking by Hirt and Nichols.

References:

- Harlow, F. H. and Welch, J. E. (1965), “Numerical Calculation of
  Time-Dependent Viscous Incompressible Flow of Fluid with Free Surface,”
  *Physics of Fluids* 8, 2182–2189. [OSTI record](https://www.osti.gov/biblio/4563173)
- Hirt, C. W. and Nichols, B. D. (1981), “Volume of Fluid (VOF) Method for the
  Dynamics of Free Boundaries,” *Journal of Computational Physics* 39,
  201–225. [Journal record](https://www.sciencedirect.com/science/article/pii/0021999181901455)
- Rudman, M. (1997), “Volume-tracking methods for interfacial flow
  calculations,” *International Journal for Numerical Methods in Fluids* 24,
  671–691. [Journal record](https://onlinelibrary.wiley.com/doi/10.1002/%28SICI%291097-0363%2819970415%2924%3A7%3C671%3A%3AAID-FLD508%3E3.0.CO%3B2-9)

This pairing is consistent in the following sense: one divergence-free
staggered velocity field defines both the pressure projection and the single
shared flux through every cell face; the same oriented water-volume flux is
subtracted from one cell and added to its neighbor. FCT limits high-order
anti-diffusive corrections using each cell's available water and air capacity.
The limiter acts on face fluxes, not by clipping or renormalizing cell values.

## Units and discrete state

- Length: metres; time: seconds; density: kg/m³; pressure: Pa; velocity: m/s.
- Terrain cells are the water cells initially, with uniform spacing `h =
  0.25 m` in the shared ENG-103 fixture. The dense, fully resident grid has
  fixed dimensions and no adaptive or moving cells.
- Cell state: solid flag, liquid volume fraction `C ∈ [0,1]`, and pressure
  `p` in Pa.
- Face state: velocity `u_f` normal to that face, in m/s. Each interior face
  has one value shared by its adjacent cells.
- Water volume is `Vw = h³ Σ C` in m³; water mass is `ρ Vw` in kg, with
  `ρ = 1000 kg/m³`.
- Gravity is `g = (0,-9.81,0) m/s²`.

## Equations and boundary conditions

For constant-density incompressible liquid, the prototype advances

`∂u/∂t + (u·∇)u = -∇p/ρ + g`, and `∇·u = 0` in liquid,

and tracks liquid indicator fraction

`∂C/∂t + ∇·(C u) = 0`.

With a projected discretely divergence-free velocity, the conservative
fraction equation is the material-interface advection equation. On each
oriented face `f`, the signed swept volume is `Q_f = A Δt u_f`; the low-order
donor flux is `Q_f C_upwind`. A second-order MUSCL reconstructed flux supplies
the high-order candidate. The FCT correction is the difference between these
fluxes, limited against per-cell lower bound `0` and capacity upper bound `h³`
for the entire set of adjacent shared faces. Consequently interior fluxes
cancel exactly in the global sum, every updated cell remains bounded, and no
post-update clipping or global water renormalization is allowed. Liquid that
crosses an explicitly open domain face is counted as boundary outflow; all
other external faces are closed.

Solid faces impose zero normal velocity and zero liquid flux. At an open
liquid/air face, atmospheric pressure is the reference `p=0`; pressure is not
solved in empty cells. Projection solves the variable-topology Poisson system

`∇²p = (ρ/Δt) ∇·u*`,

then corrects each open liquid face with `u = u* - (Δt/ρ) ∇p`. Face
coefficients use the face aperture and cell spacing. Empty-neighbor faces are
Dirichlet pressure faces; solid faces are Neumann/zero-flux. Connected liquid
components are discovered explicitly. Components touching an atmospheric
free-surface face are anchored by that Dirichlet condition. Fully enclosed
components have a constant-pressure nullspace: their right-hand side is
checked for compatibility, then pressure mean is constrained to zero while
solving. Residual, iteration count, convergence state, and divergence before
and after projection are recorded.

## Step ordering and substeps

For each fixed outer interval, select `N = ceil(dt_outer / dt_stable)` with a
documented advective CFL ceiling and gravity-acceleration bound. If `N` exceeds
the configured maximum, return an overload error without advancing or
discarding time. For each equal substep:

1. Apply gravity to open vertical faces.
2. Advect staggered velocity components through a semi-Lagrangian MAC
   interpolant; this is the prototype's deliberate first-order, dissipative
   velocity-advection approximation.
3. Zero solid-normal and closed-domain-normal velocities.
4. Build connected liquid pressure regions; solve the pressure projection
   using preconditioned conjugate gradients; correct face velocity.
5. Advect `C` by shared-face FCT volume fluxes using the corrected velocity;
   account explicit open-boundary outflow and check conservation/bounds.
6. Record substep residuals, divergence, fluxes, timings, and failed
   invariants.

Optional fixture measurements can pass a cell mask to the stepper. For each
substep, the diagnostic integrates the final donor-cell volume flux and the
limited anti-diffusive correction actually applied at each shared face. Signed
inflow and outflow across the mask boundary are reported separately; the
volume-change check is `ΔV + outflow − inflow`. This is observation only and
does not alter transport.

Static resolution comparisons refine each captured voxel boundary into an
integer number of aligned child cells, preserving physical extent and solid
occupancy while reducing `h`. The current harness enables this only for the
unchanged basin and tunnel scenes. It does not support geometry edits on a
refined fixture or imply adaptive/coarse-grid support.

Substeps are equal duration, all are measured, and required work beyond the
configured substep cap is an explicit overload. Geometry changes are staged
with a candidate voxel volume and candidate grid boundary. Water-overlap
placement is rejected; both states publish only after the candidate geometry
and fluid boundary validate.

## Deliberate limits

The prototype omits viscosity, surface tension, droplets/spray, air dynamics,
rotating/moving solids, and coarse water cells. Velocity advection is
semi-Lagrangian and can dissipate momentum; the FCT scheme conserves bounded
water volume but does not reconstruct a geometric interface. Static walls are
grid aligned. One water cell per terrain cell does not establish coarse-grid
support or rotating-hull support. This method note defines an experiment, not a
claim of game readiness or of solver accuracy at an unmeasured scale.

## Pressure convergence and measured implementation

The pressure operator is matrix-free and uses the symmetric seven-point
finite-volume Laplacian. A liquid-liquid face contributes equal reciprocal
off-diagonal coefficients `-1/h^2` and matching positive diagonal terms. An
atmospheric free-surface face contributes a positive diagonal anchor. The
diagonal Jacobi preconditioner has positive entries. Atmospheric components
are positive definite; each all-Neumann enclosed component has one constant
null mode, so its compatible RHS and Krylov vectors are projected to zero
mean. The pressure solver accepts a relative tolerance plus an absolute
L2-residual floor: `max(abs_tol, rel_tol * ||r0||2)`. The floor is 1e-8 in
the pressure-equation residual norm (Pa/m^2 over active liquid rows), which
defines convergence for a near-zero right-hand side without silently accepting
an iteration-limit exit.

Pressure is warm-started from the previous substep only while the solid
boundary remains unchanged. A committed geometry boundary edit clears the
pressure guess and the liquid-set history. When the active set changes,
non-liquid pressure is zeroed and new enclosed components receive the
zero-mean gauge projection. The iteration update preserves zero-mean pressure
and residual under a compatible RHS and the pressure operator; the
preconditioned residual and search direction are explicitly reprojected,
with one final pressure gauge projection before face correction.

The diagnostic harness can write one JSONL `pressure_substep` record per
projection with component sizes/anchors, active-set changes, RHS and warm-start
residual norms, recursive and independently recomputed final residual,
residual history, iteration count, and stage timers. These tracing runs are
separate from the uninstrumented performance captures. The ENG-103 report
records the corrected 60 Hz timing normalization, scale-2 pressure profile,
tolerance sweep, and recommended next solver comparison.

## Bounded IC(0) comparison and basin acceptance revision

The retained baseline is diagonally preconditioned Jacobi PCG. The optional
IC(0) comparison uses deterministic linear-cell ordering and the same
seven-point matrix sparsity pattern, storing only lower-triangle neighbors
already present in that pattern (zero fill). Factorization is rebuilt for
every pressure projection, after liquid-component labeling and matrix assembly;
setup is timed separately and included in end-to-end projection and tick cost.
Geometry, liquid-set, solid-face, and component changes therefore cannot reuse
stale factors. A non-positive or non-finite pivot is an explicit solver error;
there is no silent Jacobi fallback and no dependency version change.

Atmosphere-anchored components factor directly. Each enclosed all-Neumann
component selects its lowest linear cell as a deterministic factor-only gauge
pin: that row is a unit diagonal, edges to it are removed from the principal
minor, and remaining rows are factored. The physical pressure operator stays
unchanged; compatible RHS and Krylov vectors still use the existing zero-mean
projection, and preconditioner application is projected back to that subspace.
A one-cell enclosed component is represented only by the factor pin and has
zero projected residual.

Before comparative captures, the corrected stability criterion is declared:
in the final third of the 3-second flat-basin run, volume-weighted p95
cell-centred speed over cells with fraction C >= 1e-3 is at most 0.5 m/s; at
most 1% of that eligible water volume may exceed 0.5 m/s; late-window kinetic
energy rise remains <=1 J; surface p95 drift remains <0.15 m; and intact-solid
crossings remain forbidden. C=1e-3 is 0.1% of a 0.25 m cell (15.625 mL), a
small nonzero volume floor. The runner reports these statistics for C >= 0,
1e-6, 1e-4, 1e-3, and 1e-2 to show sensitivity. The raw maximum remains
reported but no longer establishes instability by itself when the associated
fraction is effectively zero. No particles are deleted or clamped, and no
damping is added.

For the same basin capture, the unfiltered maximum outflow speed used by the
current CFL chooser is 24.159 m/s at a cell with C=1.61e-31. At C>=1e-3 the
diagnostic-only maximum is 0.153 m/s and the suggested substep count changes
from 4 to 1; across the full 180-tick basin that changes 7 otherwise selected
substeps. The actual solver remains unfiltered in this comparison. The
pressure active set also retains every positive fraction: an average of 1,037
pressure rows per substep in this basin have C<1e-3. Face-adjacent cells with
C>=1e-3 beside cells below C=1e-6 have weighted p95 speed 0.025 m/s, maximum
0.040 m/s, and zero adjacent water volume above 0.5 m/s. This distinguishes
costly tiny-cell rows and their direct CFL effect from measured motion in their
occupied neighbors.

The method remains a fixed solid-boundary, gravity-driven feasibility model.
It does not compute two-way momentum transfer, pressure forces, or immersed
boundaries for moving bodies; no result here establishes boat or rigid-body
coupling.

## Bounded MAC scaling and conservation follow-up (2026-09-27)

The final scaling investigation keeps pressure participation unchanged: every cell with C>0 remains in the matrix. Eight diagnostic fraction bands report cell counts, represented water volume, pressure rows, and velocity/outflow maxima. In the 30-second flat basin, cells with 0<C<1e-8 averaged 899 to 588 rows in the six five-second windows while containing only about 2–3e-9 m3. Diagnostic cutoffs can split connected components, so dropping these cells is a topology change and requires a separate explicit experiment with preserved baseline. Highest speed at C=8.7e-34 contributed seven extra selected substeps over the first 300 ticks (307 actual versus 300 estimated at C>=1e-3); no live CFL filtering is enabled.

An applied negative FCT anti-flux at the open top was missing from the signed outflow tally, producing a disagreement between liquid-state mass loss and cumulative boundary accounting. The tally now includes the signed anti-flux used to update the state and rejects invalid negative/non-finite net outflow before publishing the update. A focused test verifies the negative anti-flux case and volume balance. The transverse reconstruction sample coordinates now offset only along the face-normal upwind axis; this fixes y/z face stencils. Neither change clamps or removes fluid.

The matched IC(0) scale-2 cost is 37.324 ms per normalized 1/60-second interval (actual outer-tick p50/p95 38.900/45.882 ms) with 14.3 MiB peak sampled process working set, versus Jacobi 49.881 ms normalized and 11.0 MiB. Although IC(0) converges, it is still 18.7x the 2 ms target. Pressure factor setup is only 4.8% of the traced aggregate pressure cost; exact factor caching has an upper bound of roughly 2% of all-in cost and is skipped. Pressure preconditioner application, vector operations, and allocator churn dominate the measured scale-2 costs.

The corrected 30-second basin run conserves mass to 1.10e-16 relative error but still drifts 0.22965 m in surface p95 and gains 16.8 kJ of mechanical energy. Late-window p95 speed is 0.175 m/s with no eligible water above 0.5 m/s, while an early raw maximum of 18.96 m/s is confined to a C=8.7e-34 cell and adds seven early substeps. The unchanged short basin gate also fails at 0.153515 m surface drift (>0.15 m) and 1.103 J energy rise (>1 J). Recommendation is one bounded active-set/representation investigation, not threshold removal by assumption. Production integration remains blocked.

Timing reporting note: the matched release table reports all-in normalized mean and all-in tick percentiles; each raw tick record separately provides solver wall time, wrapper/diagnostic overhead and boundary-update time. For these no-edit fixtures boundary rebuild measured 0 ms/tick. Pressure-stage tracing was a separate instrumented run and is excluded from untraced benchmark medians. At scale 2 IC(0), solver mean is 36.906 ms and all-in normalized mean is 37.324 ms; wrapper overhead mean/p50/p95 is 0.427/0.371/0.580 ms, and all-in outer-tick p50/p95 is 38.900/45.882 ms. The capture’s actual build flags and settings are in each grid_config record.

## Basin equilibrium and interface consistency investigation (2026-09-27)

The basin initializer previously packed the exact water volume in x/z row order: 129 full cells and one partial cell across 54 columns, with column depths spanning 0.5–0.75 m. That is a stepped initial surface, not a level reservoir. Initialization now distributes the same VOF mass uniformly over all wetted columns: depth 0.600000009 m per column, two full cells plus a C=0.400000036 surface cell. A focused test verifies per-column volume, total conservation, and pressure-row participation. Pressure participation is explicitly one nonsolid row for every C>0 cell; no small fraction cutoff is used.

The uniform physical reservoir still leaves the discrete equilibrium on its first tick. Gravity raises the volume-weighted staggered-face kinetic energy by 21.43 J. Semi-Lagrangian velocity advection leaves that value essentially unchanged. The first pressure projection leaves 9.15 J, pressure mean 2,148 Pa and max absolute pressure 5,040 Pa, with 0.135 m/s downward speed on a wet/air cell face. Fraction transport then creates 36 additional wet cells and 12 new wetted columns. The first positive upward face speed is recorded after projection at tick 1 (0.0333 s): 0.0238 m/s adjacent to partial water. Face values are proxies; the scalar fraction does not locate an actual subcell interface.

Pressure and transport use different interface descriptions. The pressure active set treats every C>0 cell as a full divergence row, independent of water fraction. Any adjacent C=0 cell anchors p=0 at the shared cell face using a half-cell stencil. FCT instead transports conservative C with applied signed face flux. Projection precedes transport, so cells first wetted during transport join the pressure matrix on the next substep. These choices do not provide geometric interface consistency: the partial cell's true free surface lies inside it, while projection anchors at the dry-neighbor cell face. Pressure convergence certifies only this assembled binary-mask matrix.

The repeated 30-second release IC(0) basin run conserves mass to 7.95e-16 relative final error (max absolute balance error 9.11e-15 m3), yet permits 0.309058 m3 through the open top. First any-positive flux is tick 204 (3.417 s) at 1e-12 m3; first >1e-9 m3/tick is tick 376 (6.283 s); >1e-4 m3/tick begins tick 1111 (18.533 s). Surface p95 drifts 1.026286 m, from 0.600000 to 1.626286 m. Represented kinetic energy rises 8.70 J and potential energy 15.07 kJ; note that potential energy assigns fractional cell mass to cell centers and cannot recover subcell interface centroids. All 1,805 pressure projections converge, with 2,501,634 total row-substeps, ~1,390 rows per outer tick, 33.66 mean iterations per outer tick, and maximum L2 residual 0.4044 Pa/m2. Total normalized cost is 1.6205 ms per 1/60-second interval. The short 3.5 mm miss therefore understated the long-run failure.

An independent read-only numerical review agrees that the current model has a credible structural free-surface inconsistency, but this evidence does not prove it is the only cause of the energy/outflow growth. The single bounded representation experiment was the conservative uniform-column seed while retaining all C>0 pressure rows. It did not settle. A principled pressure/transport replacement needs a shared reconstructed interface (such as PLIC), pressure face apertures and subcell atmospheric distances from that same interface, geometric swept-volume fluxes, and explicit stabilization for cut-cell slivers. Stop here pending numerical-method review/reference comparison; do not apply a fraction threshold or arbitrary row rescaling. Candidate remains worth keeping; production integration remains blocked.
