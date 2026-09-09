//! T10 phase A: the authoritative → replica wire surface in `spall_sim`.
//!
//! A committed [`TopologyTransaction`] must be *self-describing* — a replica with
//! no structural code reconstructs the exact committed geometry from the ops
//! alone (`docs/protocol.md`). These tests take a real split committed through
//! [`Simulation`], round-trip the transaction through the codec, and replay its
//! ops into bare [`spall_voxel`] volumes, asserting the solid-cell sets match the
//! authoritative world. Full canonical-hash parity under packet loss is the
//! `spall_client` replica's acceptance test (phase B).

use std::collections::BTreeMap;

use spall_core::{CELLS_PER_BRICK, GlobalCell, LocalCell, MaterialId, Tick, VolumeId};
use spall_protocol::{
    Record, RepairKey, RepairRequest, TopologyOp, TopologyTransaction, decode_topology,
    encode_control,
};
use spall_sim::fixtures;
use spall_sim::{
    EditIntent, EditTarget, MotionPublisher, Simulation, SimulationConfig, repair_ops,
};
use spall_voxel::{EditPlan, Volume};

use spall_core::units::{BRUSH_UNIT, BrushPoint};

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> spall_core::SphereBrush {
    let h = BRUSH_UNIT / 2;
    spall_core::SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn actor() -> spall_core::EntityId {
    spall_core::EntityId::new(1).unwrap()
}

fn solid_cells(v: &Volume) -> std::collections::BTreeSet<(i64, i64, i64)> {
    let mut out = std::collections::BTreeSet::new();
    for c in v.resident_brick_coords() {
        let s = v.snapshot_brick(c).unwrap().unwrap();
        for i in 0..CELLS_PER_BRICK as u16 {
            let lc = LocalCell::from_linear_index(i).unwrap();
            if !s.get(lc).is_air() {
                let g = GlobalCell::from_parts(c, lc).unwrap();
                out.insert((g.x, g.y, g.z));
            }
        }
    }
    out
}

/// A deliberately minimal replica: `spall_voxel` volumes and a grouped op
/// applicator. Consecutive ops on one volume are batched into a single
/// `apply_edit` so the write grouping matches the server's commit path. This is
/// the kernel that `spall_client::replica` will grow in phase B.
#[derive(Default)]
struct Replica {
    volumes: BTreeMap<u64, Volume>,
}

impl Replica {
    fn install(&mut self, volume: Volume) {
        self.volumes.insert(volume.id().get(), volume);
    }

    fn apply(&mut self, tx: &TopologyTransaction) {
        // Group consecutive same-volume writes; a `SplitOff` first stands up the
        // (initially empty) child volume, unbounded like the source.
        let mut pending: Option<(VolumeId, EditPlan)> = None;
        let flush = |pending: &mut Option<(VolumeId, EditPlan)>,
                     volumes: &mut BTreeMap<u64, Volume>| {
            if let Some((vid, plan)) = pending.take() {
                volumes
                    .get_mut(&vid.get())
                    .expect("write targets an installed volume")
                    .apply_edit(&plan)
                    .expect("replayed edit is in bounds");
            }
        };
        for op in &tx.ops {
            match op {
                TopologyOp::SplitOff { child, .. } => {
                    flush(&mut pending, &mut self.volumes);
                    self.volumes
                        .entry(child.get())
                        .or_insert_with(|| Volume::new(*child, spall_core::CellSizeCode::Quarter));
                }
                TopologyOp::IntegerBrush {
                    volume,
                    brush,
                    material,
                } => {
                    flush(&mut pending, &mut self.volumes);
                    let plan = EditPlan::sphere(*volume, *brush, *material);
                    self.volumes
                        .get_mut(&volume.get())
                        .expect("brush targets an installed volume")
                        .apply_edit(&plan)
                        .expect("replayed brush is in bounds");
                }
                TopologyOp::CellRun {
                    volume,
                    start,
                    len,
                    material,
                } => {
                    if pending.as_ref().map(|(v, _)| *v) != Some(*volume) {
                        flush(&mut pending, &mut self.volumes);
                        pending = Some((*volume, EditPlan::new(*volume)));
                    }
                    let plan = &mut pending.as_mut().unwrap().1;
                    for i in 0..*len as i64 {
                        plan.set(GlobalCell::new(start.x + i, start.y, start.z), *material);
                    }
                }
                // This mini-applier only covers the inline-encoded scenes it
                // drives; the compressed-blob split path (T17) has its own tests.
                TopologyOp::SplitOffBaseline { .. } | TopologyOp::SourcePatchBaseline { .. } => {
                    panic!("test applier does not handle split baseline ops")
                }
            }
        }
        flush(&mut pending, &mut self.volumes);
    }

    fn volume(&self, id: VolumeId) -> &Volume {
        self.volumes.get(&id.get()).expect("volume is installed")
    }
}

/// Commit a column cut that detaches the beam, and return the sim + transaction.
fn split_sim() -> (Simulation, TopologyTransaction, VolumeId) {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();
    let req = spall_protocol::RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(10, 4, 1, 2),
    ))
    .unwrap();
    sim.run_until_idle(16).unwrap();
    let tx = sim
        .committed(req)
        .expect("the column cut commits")
        .topology
        .clone();
    (sim, tx, terrain)
}

#[test]
fn a_split_transaction_round_trips_through_the_codec() {
    let (sim, tx, _) = split_sim();
    let manifest = fixtures::stone_manifest();
    let _ = &sim;

    let bytes = encode_control(&tx).expect("valid transaction encodes");
    let decoded: TopologyTransaction =
        decode_topology(&bytes, &manifest).expect("decodes against the manifest");
    assert_eq!(decoded, tx);

    // The ops carry the split explicitly: a brush, a SplitOff, and cell runs.
    assert!(matches!(tx.ops[0], TopologyOp::IntegerBrush { .. }));
    assert!(
        tx.ops
            .iter()
            .any(|o| matches!(o, TopologyOp::SplitOff { .. }))
    );
    assert!(
        tx.ops
            .iter()
            .filter(|o| matches!(o, TopologyOp::CellRun { .. }))
            .count()
            >= 2,
        "child fill + source removal both encoded as runs"
    );
    // One result hash per affected volume (source + the child), all non-zero.
    assert_eq!(tx.result_hashes.len(), 2);
    assert!(
        tx.result_hashes
            .iter()
            .all(|h| h.hash != spall_protocol::Hash32::ZERO)
    );
}

#[test]
fn replaying_the_ops_reproduces_the_authoritative_geometry() {
    let (sim, tx, terrain) = split_sim();

    // The replica's baseline is the same fixture, installed before play.
    let mut replica = Replica::default();
    replica.install(fixtures::bridged_terrain_setup().terrain);
    replica.apply(&tx);

    // Source geometry matches cell-for-cell.
    assert_eq!(
        solid_cells(replica.volume(terrain)),
        solid_cells(&sim.world().volume_ref(terrain).unwrap().clone()),
        "replica terrain equals authoritative terrain after the cut"
    );

    // Every child volume matches the authoritative body's geometry.
    let child_vids: Vec<VolumeId> = tx
        .ops
        .iter()
        .filter_map(|o| match o {
            TopologyOp::SplitOff { child, .. } => Some(*child),
            _ => None,
        })
        .collect();
    assert_eq!(child_vids.len(), 1, "the beam detaches as one body");
    for child in child_vids {
        let authoritative = sim
            .world()
            .bodies()
            .find(|b| b.volume_id == child)
            .expect("authoritative child body exists");
        assert_eq!(
            solid_cells(replica.volume(child)),
            solid_cells(&authoritative.volume.clone()),
            "replica child geometry equals the authoritative body"
        );
        assert!(!solid_cells(replica.volume(child)).is_empty());
    }

    // Conservation across the replica equals conservation across the server.
    let replica_total: usize = replica.volumes.values().map(|v| solid_cells(v).len()).sum();
    assert_eq!(replica_total as u64, sim.world().total_solid_cells());
}

#[test]
fn a_repeated_transaction_is_idempotent_on_the_replica() {
    let (_sim, tx, terrain) = split_sim();
    let mut replica = Replica::default();
    replica.install(fixtures::bridged_terrain_setup().terrain);

    replica.apply(&tx);
    let after_first = solid_cells(replica.volume(terrain));
    // A naive re-apply must not change geometry: the brush is over air now and
    // the removal runs set already-air cells to air.
    replica.apply(&tx);
    assert_eq!(after_first, solid_cells(replica.volume(terrain)));
}

#[test]
fn motion_publisher_emits_one_snapshot_per_body_at_the_motion_rate() {
    let (mut sim, _tx, _) = split_sim();
    // Let the detached beam fall a little so it has real motion.
    for _ in 0..6 {
        sim.step_physics_only();
    }
    let mut pub_ = MotionPublisher::new(60, 20);
    assert!(pub_.due(Tick(0)));
    assert!(pub_.due(Tick(3)));
    assert!(!pub_.due(Tick(1)));

    let snaps = pub_.snapshots(sim.world(), Tick(3));
    assert_eq!(snaps.len(), sim.world().body_count());
    assert!(sim.world().body_count() >= 1);
    // Sequences are strictly increasing across the batch.
    for w in snaps.windows(2) {
        assert!(w[1].snapshot_seq.0 > w[0].snapshot_seq.0);
    }
    for s in &snaps {
        s.validate().expect("published snapshot is a valid record");
    }
}

#[test]
fn repair_reply_reconstructs_a_diverged_brick() {
    let (sim, _tx, terrain) = split_sim();
    let world = sim.world();

    // Pick a resident terrain brick and ask for its repair.
    let coord = world
        .volume_ref(terrain)
        .unwrap()
        .resident_brick_coords()
        .into_iter()
        .next()
        .unwrap();
    let request = RepairRequest {
        key: RepairKey::Brick {
            volume: terrain,
            coord,
        },
        expected_revision: spall_core::Revision(0),
        current_revision: spall_core::Revision(0),
        expected_hash: spall_protocol::Hash32::ZERO,
        current_hash: spall_protocol::Hash32::ZERO,
    };
    let ops = repair_ops(world, &request).expect("brick repair is served");
    assert!(!ops.is_empty());

    // Apply the repair runs onto a wrong-but-same-shape replica brick and check
    // the cells now match the authoritative volume within that brick.
    let mut replica = Replica::default();
    replica.install(fixtures::bridged_terrain_setup().terrain);
    // Diverge: clear the whole brick on the replica.
    {
        let v = replica.volumes.get_mut(&terrain.get()).unwrap();
        let mut clear = EditPlan::new(terrain);
        for i in 0..CELLS_PER_BRICK as u16 {
            let lc = LocalCell::from_linear_index(i).unwrap();
            clear.set(GlobalCell::from_parts(coord, lc).unwrap(), MaterialId::AIR);
        }
        v.apply_edit(&clear).unwrap();
    }
    let fake_tx = TopologyTransaction {
        transaction_id: spall_core::TransactionId::new(1).unwrap(),
        server_tick: Tick(1),
        control_seq: spall_protocol::ControlSeq(1),
        algorithm_version: 1,
        dependencies: vec![],
        before: vec![],
        after: vec![],
        ops,
        result_hashes: vec![],
    };
    replica.apply(&fake_tx);

    let in_brick = |set: &std::collections::BTreeSet<(i64, i64, i64)>| {
        set.iter()
            .filter(|(x, y, z)| GlobalCell::new(*x, *y, *z).split().0 == coord)
            .count()
    };
    assert_eq!(
        in_brick(&solid_cells(replica.volume(terrain))),
        in_brick(&solid_cells(&world.volume_ref(terrain).unwrap().clone())),
    );
}
