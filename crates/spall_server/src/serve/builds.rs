//! ENG-245: game structures placed cell by cell. A build tool's action is
//! validated like a single-cell placement; the game's [`StructureBuilder`]
//! turns the aimed cell into a structure, and the server places it a few
//! cells per tick as ordinary replicated single-cell placements, so no new
//! topology operation reaches the wire or the journal.

use super::*;
use crate::action_catalog::BuildRequest;
use spall_core::MaterialId;

/// Cells submitted per tick across all builds. Placements in one region
/// commit one per tick anyway (the rest retry), so submitting faster would
/// only crowd players' own actions out of the shared intent queue: a
/// 60-cell fire pit takes about a second.
const BUILD_CELLS_PER_TICK: usize = 2;
/// The most cells one build may ask for.
const MAX_BUILD_CELLS: usize = 512;
/// How far (cells, on each axis) a structure may reach from the aimed cell.
const MAX_BUILD_SPAN_CELLS: i64 = 16;
/// Server request IDs for build cells: the server band, clear of the
/// vegetation (`1 << 59`) and damage (low) ranges, `tick * 64 + n`.
const BUILD_REQUEST_BASE: u64 = spall_sim::SERVER_REQUEST_ID_BAND + (1 << 58);

struct PendingCell {
    cell: GlobalCell,
    material: MaterialId,
    actor: EntityId,
}

#[derive(Default)]
pub(super) struct BuildQueue {
    pending: VecDeque<PendingCell>,
}

impl BuildQueue {
    /// For a build tool, asks the game for the structure, queues all but its
    /// first empty cell, and returns that cell's placement under the
    /// request's own ID so the client gets an ordinary status. Other tools'
    /// intents pass through unchanged.
    pub(super) fn accept(
        &mut self,
        world: &SimWorld,
        catalog: &ToolCatalog,
        session: SessionId,
        player_id: Option<spall_protocol::PlayerId>,
        req: &ActionRequest,
        intent: EditIntent,
    ) -> Result<EditIntent, ActionReject> {
        let Some(builder) = catalog.builder_for(req.tool) else {
            return Ok(intent);
        };
        if intent.target != EditTarget::Terrain {
            return Err(ActionReject::Unsupported(
                "structures are built on the ground",
            ));
        }
        let anchor = brush_cell(intent.brush);
        let cells = builder(&BuildRequest {
            player_id,
            slot: session.slot().0,
            tool: req.tool,
            cell: anchor,
            facing: facing(req.aim_dir),
        })
        .map_err(ActionReject::Unsupported)?;
        if cells.len() > MAX_BUILD_CELLS
            || cells.iter().any(|(cell, _)| {
                [cell.x - anchor.x, cell.y - anchor.y, cell.z - anchor.z]
                    .iter()
                    .any(|d| d.abs() > MAX_BUILD_SPAN_CELLS)
            })
        {
            return Err(ActionReject::Unsupported(
                "structure is larger than the server allows",
            ));
        }
        let terrain = &world.terrain().volume;
        let mut empty = cells
            .into_iter()
            .filter(|(cell, _)| placeable(terrain, *cell));
        let Some((first, material)) = empty.next() else {
            return Err(ActionReject::Unsupported("no room to build here"));
        };
        self.pending
            .extend(empty.map(|(cell, material)| PendingCell {
                cell,
                material,
                actor: intent.actor,
            }));
        Ok(EditIntent {
            kind: EditKind::Place(material),
            brush: single_cell(first),
            ..intent
        })
    }

    /// Submits up to [`BUILD_CELLS_PER_TICK`] queued cells that are still
    /// empty; anything filled since the build was accepted is skipped.
    pub(super) fn submit_due(&mut self, sim: &mut Simulation) {
        let tick = sim.current_tick().get();
        let mut submitted = 0;
        while submitted < BUILD_CELLS_PER_TICK {
            let Some(next) = self.pending.pop_front() else {
                return;
            };
            if !placeable(&sim.world().terrain().volume, next.cell) {
                continue;
            }
            let intent = EditIntent {
                request_id: RequestId(
                    BUILD_REQUEST_BASE + tick.wrapping_mul(64) + submitted as u64,
                ),
                actor: next.actor,
                target: EditTarget::Terrain,
                kind: EditKind::Place(next.material),
                brush: single_cell(next.cell),
                explosion: None,
            };
            match sim.submit(intent) {
                Ok(_) => submitted += 1,
                Err(spall_sim::IntentError::QueueFull { .. }) => {
                    self.pending.push_front(next);
                    return;
                }
                Err(error) => {
                    tracing::warn!(?error, cell = ?next.cell, "build cell refused");
                }
            }
        }
    }
}

/// A build places only into air (or a brick not yet allocated, which is air
/// until written); it never overwrites terrain or an earlier structure.
fn placeable(terrain: &spall_voxel::Volume, cell: GlobalCell) -> bool {
    matches!(
        terrain.sample(cell),
        Ok(spall_voxel::Sample::Empty { .. }
            | spall_voxel::Sample::Unknown(spall_voxel::Residency::Absent))
    )
}

fn brush_cell(brush: SphereBrush) -> GlobalCell {
    let cell = |units: i64| units.div_euclid(BRUSH_UNIT);
    GlobalCell::new(
        cell(brush.centre.x),
        cell(brush.centre.y),
        cell(brush.centre.z),
    )
}

fn single_cell(cell: GlobalCell) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(
            cell.x * BRUSH_UNIT + h,
            cell.y * BRUSH_UNIT + h,
            cell.z * BRUSH_UNIT + h,
        ),
        0,
    )
    .expect("a zero radius is always valid")
}

/// The aim's horizontal direction in quarter turns (see
/// [`BuildRequest::facing`]).
fn facing(aim: [f32; 3]) -> u8 {
    let [x, _, z] = aim;
    if x.abs() > z.abs() {
        if x > 0.0 { 1 } else { 3 }
    } else if z > 0.0 {
        2
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolRule;
    use std::sync::Arc;

    fn catalog(builder: crate::action_catalog::StructureBuilder) -> ToolCatalog {
        ToolCatalog::new(
            1,
            [
                ToolRule {
                    id: 0,
                    kind: EditKind::Cut,
                    max_radius_cells: 8,
                    reach_m: 12.0,
                },
                ToolRule {
                    id: 9,
                    kind: EditKind::Place(MaterialId(1)),
                    max_radius_cells: 0,
                    reach_m: 12.0,
                },
            ],
        )
        .unwrap()
        .with_build_tools([9], builder)
        .unwrap()
    }

    fn place_intent(cell: GlobalCell) -> EditIntent {
        EditIntent {
            request_id: RequestId(7),
            actor: EntityId::new(1).unwrap(),
            target: EditTarget::Terrain,
            kind: EditKind::Place(MaterialId(1)),
            brush: single_cell(cell),
            explosion: None,
        }
    }

    fn build_request(tool: u16) -> ActionRequest {
        ActionRequest {
            request_id: RequestId(7),
            input_seq: spall_protocol::InputSeq(0),
            action: ActionKind::Place,
            tool,
            aim_origin_m: [0.0; 3],
            aim_dir: [0.9, -0.3, 0.1],
            claimed_target: ClaimedTarget::Terrain,
            claimed_brush: single_cell(GlobalCell::new(0, 0, 0)),
        }
    }

    /// The walk arena: a floor with air above it.
    fn arena() -> Simulation {
        Scene::Walk.simulation()
    }

    fn air_above_floor(sim: &Simulation) -> GlobalCell {
        let terrain = &sim.world().terrain().volume;
        (0..64)
            .map(|y| GlobalCell::new(8, y, 8))
            .find(|cell| {
                matches!(terrain.sample(*cell), Ok(spall_voxel::Sample::Empty { .. }))
                    && matches!(
                        terrain.sample(GlobalCell::new(8, cell.y - 1, 8)),
                        Ok(spall_voxel::Sample::Filled(_))
                    )
            })
            .expect("the walk arena has a floor at x = z = 8")
    }

    #[test]
    fn a_build_places_the_games_cells_over_ticks_and_skips_filled_ones() {
        let mut sim = arena();
        let anchor = air_above_floor(&sim);
        let seen = Arc::new(std::sync::Mutex::new(None));
        let seen_by_builder = seen.clone();
        let tower: crate::action_catalog::StructureBuilder = Arc::new(move |request| {
            *seen_by_builder.lock().unwrap() = Some(*request);
            Ok((0..40)
                .map(|i| {
                    // One cell below the anchor is floor: never overwritten.
                    let cell = GlobalCell::new(
                        request.cell.x + i % 4,
                        request.cell.y - 1 + i / 4 % 2,
                        request.cell.z + i / 8,
                    );
                    (cell, MaterialId(3))
                })
                .collect())
        });
        let catalog = catalog(tower);
        let mut queue = BuildQueue::default();
        let first = queue
            .accept(
                sim.world(),
                &catalog,
                SessionId::from_parts(SlotId(2), 1),
                None,
                &build_request(9),
                place_intent(anchor),
            )
            .expect("built");
        let request = seen.lock().unwrap().expect("asked the game");
        assert_eq!((request.cell, request.facing, request.slot), (anchor, 1, 2));
        assert_eq!(first.request_id, RequestId(7), "the client's own request");
        assert_eq!(first.kind, EditKind::Place(MaterialId(3)));
        let floor_cells = 20;
        assert_eq!(queue.pending.len(), 40 - floor_cells - 1);

        sim.submit(first).unwrap();
        for _ in 0..40 {
            queue.submit_due(&mut sim);
            sim.tick().unwrap();
        }
        assert!(queue.pending.is_empty());
        let terrain = &sim.world().terrain().volume;
        for i in 0..40 {
            let cell =
                GlobalCell::new(anchor.x + i % 4, anchor.y - 1 + i / 4 % 2, anchor.z + i / 8);
            let sample = terrain.sample(cell).unwrap();
            if cell.y == anchor.y {
                assert_eq!(
                    sample,
                    spall_voxel::Sample::Filled(MaterialId(3)),
                    "{cell:?}"
                );
            } else {
                assert_ne!(
                    sample,
                    spall_voxel::Sample::Filled(MaterialId(3)),
                    "floor kept: {cell:?}"
                );
            }
        }
    }

    #[test]
    fn refusals_and_oversized_structures_are_rejected_and_plain_tools_pass() {
        let sim = arena();
        let anchor = air_above_floor(&sim);
        let session = SessionId::from_parts(SlotId(0), 1);
        let refuse = catalog(Arc::new(|_| Err("needs 4 stones")));
        let mut queue = BuildQueue::default();
        assert_eq!(
            queue.accept(
                sim.world(),
                &refuse,
                session,
                None,
                &build_request(9),
                place_intent(anchor)
            ),
            Err(ActionReject::Unsupported("needs 4 stones"))
        );
        let sprawling = catalog(Arc::new(|request| {
            Ok(vec![(
                GlobalCell::new(request.cell.x + 40, request.cell.y, request.cell.z),
                MaterialId(3),
            )])
        }));
        assert!(
            queue
                .accept(
                    sim.world(),
                    &sprawling,
                    session,
                    None,
                    &build_request(9),
                    place_intent(anchor)
                )
                .is_err()
        );
        let plain = place_intent(anchor);
        assert_eq!(
            queue.accept(
                sim.world(),
                &refuse,
                session,
                None,
                &build_request(0),
                plain
            ),
            Ok(plain)
        );
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn only_single_cell_placement_tools_can_build() {
        let builder: crate::action_catalog::StructureBuilder = Arc::new(|_| Ok(Vec::new()));
        let base = ToolCatalog::new(
            1,
            [ToolRule {
                id: 0,
                kind: EditKind::Cut,
                max_radius_cells: 0,
                reach_m: 12.0,
            }],
        )
        .unwrap();
        assert!(base.clone().with_build_tools([0], builder.clone()).is_err());
        assert!(base.with_build_tools([5], builder).is_err());
    }

    #[test]
    fn facing_is_the_aims_nearest_quarter_turn() {
        assert_eq!(facing([0.0, -1.0, -0.2]), 0);
        assert_eq!(facing([0.7, 0.0, 0.2]), 1);
        assert_eq!(facing([0.1, 0.0, 0.9]), 2);
        assert_eq!(facing([-0.7, 0.0, -0.6]), 3);
    }
}
