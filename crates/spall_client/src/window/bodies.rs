use super::*;

/// Runs [`build_body_instances`] on its own dedicated background thread, at
/// roughly 60 Hz, unconditionally (no request/distance gating — unlike
/// [`RebuildWorker`], bodies need to look freshly-moved every frame, not
/// only after the *player* has moved far). Exists because a first version of
/// body rendering called `build_body_instances` directly in
/// `RedrawRequested`, gated only by a non-blocking `try_lock` on the shared
/// `Mutex<ReplicaWorld>` so it could never *stall waiting for* the lock --
/// but the walk itself (per body: extract its resident bricks, sample every
/// near cell, and for each solid one sample all six neighbours again for the
/// buried-cell check) is real per-frame CPU work, and with a full
/// [`crate::playground`] debris population (~200 bodies) it was measurably
/// too much to redo on the render thread every frame: average frame time
/// climbed past 100 ms while the renderer's own internal timing stayed
/// under 20 ms the whole time -- the cost was real, just outside what that
/// measurement covers. Same fix as terrain's own `RebuildWorker` for the
/// identical shape of problem: move the walk off the thread that has to hit
/// 60 fps, and let the render thread just draw whatever the worker most
/// recently finished.
pub(super) struct BodyWorker {
    pub(super) result_rx: mpsc::Receiver<Vec<BodyDraw>>,
}

impl BodyWorker {
    pub(super) fn spawn(session: Arc<InteractiveSession>) -> Result<Self, ClientError> {
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("spall-client-bodies".into())
            .spawn(move || {
                let mut templates = BodyTemplates::new();
                loop {
                    // Two phases, deliberately: hold the lock only long
                    // enough to read each body's pose (and clone its small
                    // volume *only* when its topology changed since the last
                    // pass — `snapshot_bodies`), then release it *before*
                    // doing any per-cell work (`collect_body_draws`) — see
                    // `snapshot_bodies`'s doc for why the walk itself must
                    // never run while this lock is held.
                    let now = Instant::now();
                    let local_poses = session
                        .local_body_poses
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let views = match session.replica.get() {
                        Some(replica) => {
                            let focus_m = session
                                .view
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .map(|v| v.predicted.position_m);
                            let mut replica = replica.lock().unwrap_or_else(|e| e.into_inner());
                            let render_tick = replica.render_tick(now);
                            snapshot_bodies(
                                &replica,
                                render_tick,
                                focus_m,
                                local_poses.as_ref(),
                                now,
                                &templates,
                            )
                        }
                        None => Vec::new(),
                    };
                    let draws = collect_body_draws(&views, &mut templates);
                    if result_tx.send(draws).is_err() {
                        return; // the window is gone
                    }
                    // Not a hard 60 Hz guarantee (the build above takes real
                    // time too), just a floor so this never busy-spins faster
                    // than the render thread could possibly use it.
                    std::thread::sleep(Duration::from_millis(4));
                }
            })
            .map_err(|e| ClientError::Gpu(format!("spawning the body-instance thread: {e}")))?;
        Ok(Self { result_rx })
    }
}

/// One body as [`BodyWorker`] sees it for a single pass: identity, the
/// topology revision its cached template is keyed on, its volume (cloned only
/// on a template miss), and where to draw it.
pub(super) struct BodyView {
    entity: u64,
    revision: u64,
    volume: Option<Volume>,
    translation_m: [f64; 3],
    /// Unit quaternion `[x, y, z, w]`.
    rotation: [f32; 4],
    /// Network bodies only: what the renderer needs to re-pose at draw time.
    net: Option<NetPose>,
}

/// A replicated body's motion history plus the render clock and focus it was
/// sampled against, so the render thread can pose it at its own frame time.
#[derive(Clone)]
pub(super) struct NetPose {
    sampler: crate::replica::BodySampler,
    /// Fractional server tick of the replica's render clock at `sampled_at`.
    tick_at_sample: f64,
    sampled_at: Instant,
    focus_m: Option<[f64; 3]>,
}

impl NetPose {
    fn render_tick(&self, now: Instant) -> f64 {
        self.tick_at_sample
            + now.saturating_duration_since(self.sampled_at).as_secs_f64() * self.sampler.hz()
    }
}

/// Per-body greedy mesh templates, keyed by topology revision.
/// The third field is the body-local pivot ([`body_pivot_m`]) poses move about.
pub(super) type BodyTemplates =
    std::collections::HashMap<u64, (u64, Arc<spall_mesh::Mesh>, [f64; 3])>;

/// A body's cached mesh plus the pose the worker last saw for it.
pub(super) struct BodyDraw {
    entity: u64,
    #[cfg(test)]
    #[allow(dead_code)]
    template: Arc<Vec<Instance>>,
    mesh: Arc<spall_mesh::Mesh>,
    /// Body-local point near the centre of mass; see [`body_pivot_m`].
    pivot_m: [f64; 3],
    translation_m: [f64; 3],
    rotation: [f32; 4],
    net: Option<NetPose>,
}

/// Every live body's pose (and, only when its cached template is missing or
/// stale, a clone of its volume), read from the locked replica —
/// deliberately the *only* thing [`BodyWorker`] does while holding the lock,
/// same reason [`RebuildWorker`] clones the terrain volume out instead of
/// walking it locked (see that struct's doc). An earlier version had
/// [`build_body_instances`] itself take `&ReplicaWorld` and do the whole
/// per-cell walk while still holding the lock: moving that walk to its own
/// thread stopped it from stalling the *render* thread, but the walk still
/// held the lock for its entire duration every ~16 ms, which measurably
/// stalled the *network/prediction* thread instead — same underlying mistake
/// (expensive work performed while a shared lock most other threads also
/// need is held), just relocated to a different pair of threads and a
/// different symptom (predicted movement stuttering — "move, pause, move,
/// pause" while holding a direction key — instead of dropped frames).
///
/// `local_poses` (testing-only client authority) is sampled at `now`, so
/// locally simulated bodies interpolate between fixed physics steps.
pub(super) fn snapshot_bodies(
    replica: &ReplicaWorld,
    render_tick: f64,
    focus_m: Option<[f64; 3]>,
    local_poses: Option<&crate::interactive::LocalBodyPoses>,
    now: Instant,
    templates: &BodyTemplates,
) -> Vec<BodyView> {
    replica
        .body_volumes()
        .filter_map(|(entity, volume_id)| {
            let volume = replica.volume(volume_id)?;
            let raw = entity.get();
            let mut net = None;
            let (translation_m, rotation) =
                match local_poses.and_then(|poses| poses.sample(raw, now)) {
                    Some(pose) => (pose.translation_m, pose.rotation),
                    None => {
                        let sampler = replica.body_sampler(entity)?;
                        let pivot_m = templates.get(&raw).map_or([0.0; 3], |t| t.2);
                        let pose = sampler.presented_about(render_tick, focus_m, pivot_m)?;
                        net = Some(NetPose {
                            sampler,
                            tick_at_sample: render_tick,
                            sampled_at: now,
                            focus_m,
                        });
                        (
                            pose.translation_m,
                            pose.rotation.to_unit().unwrap_or([0.0, 0.0, 0.0, 1.0]),
                        )
                    }
                };
            let revision = volume.next_revision().get();
            let cached = templates
                .get(&raw)
                .is_some_and(|(rev, _, _)| *rev == revision);
            Some(BodyView {
                entity: raw,
                revision,
                volume: (!cached).then(|| volume.clone()),
                translation_m,
                rotation,
                net,
            })
        })
        .collect()
}

/// Body-local cube positions/colours for `volume` (no pose applied).
#[cfg(test)]
#[allow(dead_code)]
pub(super) fn build_body_template(volume: &Volume) -> Vec<Instance> {
    let mut instances = Vec::new();
    let bricks = volume.resident_brick_coords();
    if bricks.is_empty() {
        return instances;
    }
    let (mut min, mut max) = (bricks[0], bricks[0]);
    for b in &bricks {
        min.x = min.x.min(b.x);
        min.y = min.y.min(b.y);
        min.z = min.z.min(b.z);
        max.x = max.x.max(b.x);
        max.y = max.y.max(b.y);
        max.z = max.z.max(b.z);
    }
    let edge = spall_core::BRICK_EDGE as i64;
    let cell_min = GlobalCell::new(min.x * edge, min.y * edge, min.z * edge);
    let cell_max = GlobalCell::new(
        max.x * edge + edge - 1,
        max.y * edge + edge - 1,
        max.z * edge + edge - 1,
    );
    let cell_m = volume.cell_size().metres();
    for gz in cell_min.z..=cell_max.z {
        for gy in cell_min.y..=cell_max.y {
            for gx in cell_min.x..=cell_max.x {
                let cell = GlobalCell::new(gx, gy, gz);
                let Ok(Sample::Filled(material)) = volume.sample(cell) else {
                    continue;
                };
                // No `is_buried` check here (unlike terrain's own
                // `build_instances`): these bodies are at most a few
                // cells per axis, so buried interior cells are rare and
                // small in number, while the check itself costs up to
                // six more `volume.sample` calls per solid cell —
                // proportionally far more expensive here than for
                // terrain's much larger, mostly-interior volumes. A few
                // wasted, fully-occluded cubes are cheaper than the
                // lookups that would have culled them.
                instances.push(Instance {
                    offset: [
                        ((cell.x as f64 + 0.5) * cell_m) as f32,
                        ((cell.y as f64 + 0.5) * cell_m) as f32,
                        ((cell.z as f64 + 0.5) * cell_m) as f32,
                    ],
                    material: u32::from(material.0),
                    size: [cell_m as f32; 3],
                    _pad: 0.0,
                    rotation: IDENTITY_ROTATION,
                });
            }
        }
    }
    instances
}

pub(super) fn build_body_mesh_template(volume: &Volume) -> spall_mesh::Mesh {
    spall_mesh::build_volume_mesh(
        volume,
        Generation::START,
        TopologyEpoch::START,
        Default::default(),
    )
    .map(|built| built.mesh)
    .unwrap_or_default()
}

/// Every detached body's cached cells and last-seen pose, built from
/// [`snapshot_bodies`]'s output entirely after the replica lock has been
/// released (see that function's doc for why this split exists). Nothing in
/// this renderer built body instances before this feature (`build_instances`
/// only ever walks the *terrain* volume) — a hands-on playground session with
/// a live debris spawner found this the hard way: bodies were replicating and
/// reconciling correctly the whole time, just never once drawn.
pub(super) fn collect_body_draws(
    views: &[BodyView],
    templates: &mut BodyTemplates,
) -> Vec<BodyDraw> {
    let live: std::collections::HashSet<u64> = views.iter().map(|v| v.entity).collect();
    templates.retain(|entity, _| live.contains(entity));

    let mut draws = Vec::with_capacity(views.len());
    for view in views {
        if let Some(volume) = &view.volume {
            let mesh = Arc::new(build_body_mesh_template(volume));
            templates.insert(
                view.entity,
                (view.revision, mesh, crate::replica::body_pivot_m(volume)),
            );
        }
        let Some((_, mesh, pivot_m)) = templates.get(&view.entity) else {
            continue;
        };
        draws.push(BodyDraw {
            entity: view.entity,
            #[cfg(test)]
            template: Arc::new(Vec::new()),
            mesh: mesh.clone(),
            pivot_m: *pivot_m,
            translation_m: view.translation_m,
            rotation: view.rotation,
            net: view.net.clone(),
        });
    }
    draws
}

/// Poses every body's cached cells for drawing at `now`: the locally
/// simulated pose interpolated to `now` when there is one, else the pose the
/// worker last saw. Each cube gets the body's full rotation (its centre is
/// rotated and so is its mesh), not just its centre.
#[allow(dead_code)]
#[cfg(test)]
pub(super) fn pose_body_instances(
    draws: &[BodyDraw],
    local_poses: Option<&crate::interactive::LocalBodyPoses>,
    now: Instant,
    stats: &mut PoseStats,
    out: &mut Vec<Instance>,
) {
    for draw in draws {
        let (translation_m, rotation) = match local_poses.and_then(|p| p.sample(draw.entity, now)) {
            Some(pose) => (pose.translation_m, pose.rotation),
            None => match &draw.net {
                Some(net) => {
                    let tick = net.render_tick(now);
                    match net.sampler.presented(tick, net.focus_m) {
                        Some(pose) => {
                            if net.sampler.is_moving() {
                                let age_ms = net.sampler.latest_age_ticks(tick).unwrap_or(0.0)
                                    / net.sampler.hz()
                                    * 1000.0;
                                stats.record(draw.entity, pose.translation_m, age_ms);
                            }
                            (
                                pose.translation_m,
                                pose.rotation.to_unit().unwrap_or([0.0, 0.0, 0.0, 1.0]),
                            )
                        }
                        None => (draw.translation_m, draw.rotation),
                    }
                }
                None => (draw.translation_m, draw.rotation),
            },
        };
        let [rx, ry, rz, rw] = rotation;
        let quat = glam::Quat::from_xyzw(rx, ry, rz, rw);
        let translation = Vec3::new(
            translation_m[0] as f32,
            translation_m[1] as f32,
            translation_m[2] as f32,
        );
        out.extend(draw.template.iter().map(|cell| Instance {
            offset: (quat * Vec3::from_array(cell.offset) + translation).to_array(),
            rotation,
            ..*cell
        }));
    }
}

pub(super) fn pose_body_meshes(
    draws: &[BodyDraw],
    local_poses: Option<&crate::interactive::LocalBodyPoses>,
    now: Instant,
    stats: &mut PoseStats,
) -> (Vec<MeshUpdate<u64>>, Vec<u64>) {
    let mut output = Vec::with_capacity(draws.len());
    let mut live = Vec::with_capacity(draws.len());
    for draw in draws {
        let (translation_m, rotation) = match local_poses.and_then(|p| p.sample(draw.entity, now)) {
            Some(pose) => (pose.translation_m, pose.rotation),
            None => match &draw.net {
                Some(net) => {
                    let tick = net.render_tick(now);
                    match net.sampler.presented_about(tick, net.focus_m, draw.pivot_m) {
                        Some(pose) => {
                            if net.sampler.is_moving() {
                                let age_ms = net.sampler.latest_age_ticks(tick).unwrap_or(0.0)
                                    / net.sampler.hz()
                                    * 1000.0;
                                stats.record(draw.entity, pose.translation_m, age_ms);
                            }
                            (
                                pose.translation_m,
                                pose.rotation.to_unit().unwrap_or([0.0, 0.0, 0.0, 1.0]),
                            )
                        }
                        None => (draw.translation_m, draw.rotation),
                    }
                }
                None => (draw.translation_m, draw.rotation),
            },
        };
        let [x, y, z, w] = rotation;
        let transform = glam::Mat4::from_rotation_translation(
            glam::Quat::from_xyzw(x, y, z, w),
            Vec3::new(
                translation_m[0] as f32,
                translation_m[1] as f32,
                translation_m[2] as f32,
            ),
        );
        let (vertices, indices) = spall_render::to_gpu(&draw.mesh, transform);
        output.push((draw.entity, vertices, indices));
        live.push(draw.entity);
    }
    (output, live)
}

/// Draw-time pose diagnostics for moving network bodies: how old the newest
/// snapshot is at each draw, and how often a moving body is drawn at exactly
/// the pose it had the previous frame (a visible hold).
#[derive(Default)]
pub(super) struct PoseStats {
    last: std::collections::HashMap<u64, [f64; 3]>,
    draws: u64,
    repeats: u64,
    age_sum_ms: f64,
    age_max_ms: f64,
}

impl PoseStats {
    fn record(&mut self, entity: u64, translation_m: [f64; 3], age_ms: f64) {
        self.draws += 1;
        self.age_sum_ms += age_ms;
        self.age_max_ms = self.age_max_ms.max(age_ms);
        if self.last.insert(entity, translation_m) == Some(translation_m) {
            self.repeats += 1;
        }
    }

    /// Report text; resets the counters (not the per-entity history).
    pub(super) fn take_report(&mut self) -> String {
        let text = if self.draws == 0 {
            "net bodies: none moving".to_string()
        } else {
            format!(
                "net bodies: {} moving draws, {} repeated poses, snapshot age {:.0} ms (avg) / {:.0} ms (max)",
                self.draws,
                self.repeats,
                self.age_sum_ms / self.draws as f64,
                self.age_max_ms
            )
        };
        self.draws = 0;
        self.repeats = 0;
        self.age_sum_ms = 0.0;
        self.age_max_ms = 0.0;
        text
    }
}

/// One thin axis-aligned cuboid from `a` to `b`. Capsule-box edges are all
/// axis aligned, so this gives the cube-only debug renderer true continuous
/// wire-like strokes without adding a separate line pipeline.
pub(super) fn debug_line(a: Vec3, b: Vec3, material: u32) -> impl Iterator<Item = Instance> {
    debug_line_thick(a, b, material, 0.0125)
}

/// [`debug_line`] with an explicit stroke thickness in metres.
pub(super) fn debug_line_thick(
    a: Vec3,
    b: Vec3,
    material: u32,
    thickness_m: f32,
) -> impl Iterator<Item = Instance> {
    let delta = (b - a).abs();
    let mut dimensions = [thickness_m; 3];
    let axis = if delta.x >= delta.y && delta.x >= delta.z {
        0
    } else if delta.y >= delta.z {
        1
    } else {
        2
    };
    dimensions[axis] = delta[axis] + thickness_m;
    let midpoint = (a + b) * 0.5;
    std::iter::once(Instance {
        offset: midpoint.to_array(),
        material,
        size: dimensions,
        _pad: 0.0,
        rotation: IDENTITY_ROTATION,
    })
}

/// `F3`: a stand-in for real collision-shape wireframes. This renderer has
/// no line/wireframe pipeline at all (see the module doc: a single unit-cube
/// mesh instanced by position + colour, nothing else) -- building one is a
/// real render-pipeline change, out of scope for a debug toggle. This reuses
/// the existing instancing mechanism instead: very thin cuboids along all
/// twelve edges of the player's actual collision bounds
/// (`CharacterParams::DEFAULT`'s capsule, approximated here as its own
/// bounding box -- `0.6 m x 0.6 m` footprint, `1.8 m` tall -- since the
/// capsule's rounded ends are a much smaller visual difference than "is
/// there an outline here at all"). It is not a raster-line primitive, but the
/// 1.25 cm strokes read as a wireframe instead of voxel-sized bars and show
/// exactly where the client believes its own collision volume is relative
/// to the terrain and debris around it, which is the concrete, useful
/// question "collision outlines" is usually really asking. A first version
/// only drew the four vertical edges (no top/bottom rings connecting them),
/// which read as four floating dashed lines rather than anything box-shaped
/// -- the full 12-edge wireframe below is what actually looks like "a body
/// cube."
pub(super) fn build_capsule_debug_instances(feet_m: [f64; 3], material: u32) -> Vec<Instance> {
    let params = CharacterParams::DEFAULT;
    let r = f64::from(params.radius_m) as f32;
    let height = f64::from(params.total_height_m()) as f32;
    let feet = Vec3::new(feet_m[0] as f32, feet_m[1] as f32, feet_m[2] as f32);

    // The box's 8 corners: bottom ring (y=0) then top ring (y=height), each
    // in (+x+z, +x-z, -x-z, -x+z) order.
    let corner = |dx: f32, dz: f32, dy: f32| feet + Vec3::new(dx * r, dy * height, dz * r);
    let bottom = [
        corner(1.0, 1.0, 0.0),
        corner(1.0, -1.0, 0.0),
        corner(-1.0, -1.0, 0.0),
        corner(-1.0, 1.0, 0.0),
    ];
    let top = [
        corner(1.0, 1.0, 1.0),
        corner(1.0, -1.0, 1.0),
        corner(-1.0, -1.0, 1.0),
        corner(-1.0, 1.0, 1.0),
    ];

    let mut instances = Vec::new();
    for i in 0..4 {
        let j = (i + 1) % 4;
        instances.extend(debug_line(bottom[i], bottom[j], material)); // bottom ring
        instances.extend(debug_line(top[i], top[j], material)); // top ring
        instances.extend(debug_line(bottom[i], top[i], material)); // vertical edge
    }
    instances
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsule_debug_edges_are_thin_continuous_wire_strokes() {
        let lines = build_capsule_debug_instances([0.0; 3], 99);
        assert_eq!(lines.len(), 12, "one cuboid per bounding-box edge");
        for line in lines {
            let dimensions = line.size;
            let thin_axes = dimensions
                .into_iter()
                .filter(|dimension| *dimension <= 0.013)
                .count();
            assert_eq!(thin_axes, 2, "line dimensions were {dimensions:?}");
        }
    }

    #[test]
    fn locally_simulated_pose_transforms_body_mesh_vertices() {
        let now = Instant::now();
        let draws = [BodyDraw {
            entity: 7,
            template: Arc::new(Vec::new()),
            mesh: Arc::new(spall_mesh::Mesh {
                vertices: vec![spall_mesh::Vertex {
                    position: [0.0; 3],
                    normal: [0.0, 1.0, 0.0],
                    material: 1,
                    ao: 1.0,
                    local_uv: [0.0; 2],
                }],
                indices: Vec::new(),
            }),
            pivot_m: [0.0; 3],
            translation_m: [2.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            net: None,
        }];
        let local_pose = crate::interactive::LocalPose {
            translation_m: [8.0, 1.0, 3.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
        };
        let local = crate::interactive::LocalBodyPoses {
            prev: Default::default(),
            curr: [(7, local_pose)].into_iter().collect(),
            curr_at: now,
            step: Duration::from_millis(16),
        };
        let (meshes, live) = pose_body_meshes(&draws, Some(&local), now, &mut PoseStats::default());

        assert_eq!(live, vec![7]);
        assert_eq!(meshes.len(), 1);
        assert_eq!(meshes[0].0, 7);
        assert_eq!(meshes[0].1[0].position, [8.0, 1.0, 3.0]);
    }
}
