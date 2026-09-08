//! Minimal game-owned content for the sandbox example.
//!
//! This is the `sandbox_game` layer from `docs/architecture.md`: the engine
//! (`spall_sim` and the other `spall_*` crates) never imports it, but the
//! example is free to depend on the engine. T08 only needs enough here to turn a
//! player's tool use into an engine [`EditIntent`] — a material catalog and two
//! tools. Recipes, damage tables, entity spawn rules and asset loading are the
//! T25 handoff, not this.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    EntityId, MaterialDef, MaterialFlags, MaterialId, MaterialManifest, RenderProps, SimProps,
    SphereBrush,
};
use spall_sim::{EditIntent, EditKind, EditTarget, RequestId};

/// Stable ids for the sandbox's handful of materials. Numeric ids are fixed
/// here, never assigned by enumeration order.
pub mod materials {
    use spall_core::MaterialId;
    pub const AIR: MaterialId = MaterialId(0);
    pub const STONE: MaterialId = MaterialId(1);
    pub const DIRT: MaterialId = MaterialId(2);
    pub const WOOD: MaterialId = MaterialId(3);
}

fn def(id: MaterialId, name: &str, density: f32, albedo: [f32; 3]) -> MaterialDef {
    MaterialDef {
        id,
        name: name.into(),
        render: RenderProps {
            albedo,
            roughness: 0.85,
            metalness: 0.0,
            emissive: [0.0; 3],
        },
        sim: SimProps {
            density_kg_m3: density,
            friction: 0.8,
            restitution: 0.05,
            hardness: 3.0,
            bond_strength: 10.0,
            flags: MaterialFlags(
                MaterialFlags::OPAQUE.0 | MaterialFlags::COLLIDES.0 | MaterialFlags::STRUCTURAL.0,
            ),
        },
    }
}

/// The sandbox world's validated material manifest.
pub fn manifest() -> MaterialManifest {
    MaterialManifest::validated(vec![
        MaterialDef {
            id: materials::AIR,
            name: "air".into(),
            render: RenderProps {
                albedo: [0.0; 3],
                roughness: 1.0,
                metalness: 0.0,
                emissive: [0.0; 3],
            },
            sim: SimProps {
                density_kg_m3: 0.0,
                friction: 0.0,
                restitution: 0.0,
                hardness: 0.0,
                bond_strength: 0.0,
                flags: MaterialFlags::NONE,
            },
        },
        def(materials::STONE, "stone", 2600.0, [0.50, 0.50, 0.52]),
        def(materials::DIRT, "dirt", 1500.0, [0.35, 0.25, 0.15]),
        def(materials::WOOD, "wood", 700.0, [0.45, 0.30, 0.15]),
    ])
    .expect("sandbox manifest is valid")
}

/// A player tool: a fixed-radius sphere brush that either removes material or
/// places one material.
#[derive(Debug, Clone, Copy)]
pub struct Tool {
    pub radius_cells: i64,
    pub kind: EditKind,
}

impl Tool {
    /// The one-metre digging tool: a 2-cell-radius (`0.5 m`) cut sphere, matching
    /// the `4 x 4 x 4` cell footprint of a one-metre placement in
    /// `docs/architecture.md`.
    pub const DIG: Tool = Tool {
        radius_cells: 2,
        kind: EditKind::Cut,
    };

    /// The one-metre stone placement tool.
    pub const PLACE_STONE: Tool = Tool {
        radius_cells: 2,
        kind: EditKind::Place(materials::STONE),
    };

    /// Turns a tool use — aimed at an already-validated `(cell_x, cell_y,
    /// cell_z)` in the target volume's local cell space — into an engine
    /// [`EditIntent`]. The server still owns whether it commits.
    pub fn intent(
        &self,
        request_id: RequestId,
        actor: EntityId,
        target: EditTarget,
        cell_x: i64,
        cell_y: i64,
        cell_z: i64,
    ) -> EditIntent {
        let half = BRUSH_UNIT / 2;
        let brush = SphereBrush::new(
            BrushPoint::from_units(
                cell_x * BRUSH_UNIT + half,
                cell_y * BRUSH_UNIT + half,
                cell_z * BRUSH_UNIT + half,
            ),
            self.radius_cells * BRUSH_UNIT,
        )
        .expect("tool radius is within brush limits");
        EditIntent {
            request_id,
            actor,
            target,
            kind: self.kind,
            brush,
            explosion: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_valid_and_ids_are_fixed() {
        let m = manifest();
        assert_eq!(m.get(materials::WOOD).unwrap().name, "wood");
        assert!(m.get(MaterialId(99)).is_none());
    }

    #[test]
    fn a_tool_use_becomes_a_matching_edit_intent() {
        let intent = Tool::DIG.intent(
            RequestId(7),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            10,
            4,
            2,
        );
        assert!(matches!(intent.kind, EditKind::Cut));
        assert_eq!(intent.brush.radius_units(), 2 * BRUSH_UNIT);
        assert!(matches!(intent.target, EditTarget::Terrain));
    }
}
