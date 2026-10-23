//! ENG-245: camps. Building a fire pit founds the player's camp: a claimed
//! area of [`CAMP_RADIUS_M`] around it. Structures are real voxels, placed
//! by the server cell by cell; this module decides their cells and cost and
//! keeps each player's sites (in memory for now, like slot inventories).

use std::collections::BTreeMap;

use spall_core::{GlobalCell, MaterialId};
use spall_protocol::{PlayerId, SiteEntry};

use crate::game::{Inventory, ItemStack, items, materials};

/// Stable site kind codes sent to clients in `SiteEntry::kind`.
pub mod site_kinds {
    pub const FIRE_PIT: u16 = 1;
}

/// How far a camp reaches from its fire pit.
pub const CAMP_RADIUS_M: f32 = 15.0;

/// What a fire pit costs.
pub const FIRE_PIT_COST: [ItemStack; 1] = [ItemStack {
    item: items::STONE_CHUNK,
    count: 4,
}];

/// A fire pit with `anchor` (the empty cell on the ground) at its centre: a
/// ring of stones five cells (1.25 m) across, crossed wood inside, and a
/// glowing ember on top.
pub fn fire_pit_cells(anchor: GlobalCell) -> Vec<(GlobalCell, MaterialId)> {
    let at =
        |dx: i64, dy: i64, dz: i64| GlobalCell::new(anchor.x + dx, anchor.y + dy, anchor.z + dz);
    let mut cells = Vec::new();
    for dx in -2..=2_i64 {
        for dz in -2..=2_i64 {
            let ring = dx.abs() == 2 || dz.abs() == 2;
            let corner = dx.abs() == 2 && dz.abs() == 2;
            if ring && !corner {
                cells.push((at(dx, 0, dz), materials::STONE));
            } else if !ring && (dx == 0 || dz == 0) {
                cells.push((at(dx, 0, dz), materials::WOOD));
            }
        }
    }
    cells.push((at(0, 1, 0), materials::LAMP));
    cells
}

/// Whose sites these are: a credentialed player, or a session slot on a
/// development host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SiteOwner {
    Slot(u32),
    Player(PlayerId),
}

impl SiteOwner {
    pub fn new(player_id: Option<PlayerId>, slot: u32) -> Self {
        player_id.map_or(Self::Slot(slot), Self::Player)
    }
}

/// Every player's placed structures.
#[derive(Debug, Default)]
pub struct Camps {
    last_site_id: u64,
    sites: BTreeMap<SiteOwner, Vec<SiteEntry>>,
}

impl Camps {
    /// Founds `owner`'s camp with a fire pit at `anchor`, charging its cost.
    /// Returns the cells to place, or why not.
    pub fn found(
        &mut self,
        owner: SiteOwner,
        inventory: &mut Inventory,
        anchor: GlobalCell,
        facing: u8,
    ) -> Result<Vec<(GlobalCell, MaterialId)>, &'static str> {
        if self.camp(owner).is_some() {
            return Err("you already have a camp");
        }
        inventory
            .spend(&FIRE_PIT_COST)
            .map_err(|_| "a fire pit needs 4 stones")?;
        self.last_site_id += 1;
        self.sites.entry(owner).or_default().push(SiteEntry {
            site_id: self.last_site_id,
            kind: site_kinds::FIRE_PIT,
            cell: [anchor.x, anchor.y, anchor.z],
            facing,
        });
        Ok(fire_pit_cells(anchor))
    }

    /// `owner`'s fire pit, if they have founded a camp.
    pub fn camp(&self, owner: SiteOwner) -> Option<SiteEntry> {
        self.sites
            .get(&owner)?
            .iter()
            .find(|site| site.kind == site_kinds::FIRE_PIT)
            .copied()
    }

    /// `owner`'s sites, sorted by ID, for progression responses.
    pub fn sites(&self, owner: SiteOwner) -> Vec<SiteEntry> {
        self.sites.get(&owner).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stones(count: u32) -> Inventory {
        Inventory::from_stacks([ItemStack {
            item: items::STONE_CHUNK,
            count,
        }])
        .unwrap()
    }

    #[test]
    fn a_fire_pit_is_a_stone_ring_with_wood_and_an_ember_on_the_ground() {
        let anchor = GlobalCell::new(10, 4, -3);
        let cells = fire_pit_cells(anchor);
        let count = |material| cells.iter().filter(|(_, m)| *m == material).count();
        assert_eq!(count(materials::STONE), 12);
        assert_eq!(count(materials::WOOD), 5);
        assert_eq!(count(materials::LAMP), 1);
        assert!(
            cells
                .iter()
                .all(|(c, _)| c.y >= anchor.y && (c.x - anchor.x).abs() <= 2)
        );
        let unique: std::collections::BTreeSet<_> =
            cells.iter().map(|(c, _)| (c.x, c.y, c.z)).collect();
        assert_eq!(unique.len(), cells.len());
    }

    #[test]
    fn founding_charges_stones_once_and_records_one_camp_per_player() {
        let mut camps = Camps::default();
        let me = SiteOwner::Slot(1);
        let mut poor = stones(3);
        assert_eq!(
            camps.found(me, &mut poor, GlobalCell::new(0, 0, 0), 0),
            Err("a fire pit needs 4 stones")
        );
        assert_eq!(poor.count(items::STONE_CHUNK), 3, "nothing charged");
        assert!(camps.camp(me).is_none());

        let mut rich = stones(9);
        let cells = camps
            .found(me, &mut rich, GlobalCell::new(5, 2, 7), 3)
            .unwrap();
        assert_eq!(cells, fire_pit_cells(GlobalCell::new(5, 2, 7)));
        assert_eq!(rich.count(items::STONE_CHUNK), 5);
        let camp = camps.camp(me).unwrap();
        assert_eq!(
            (camp.kind, camp.cell, camp.facing),
            (site_kinds::FIRE_PIT, [5, 2, 7], 3)
        );
        assert_eq!(
            camps.found(me, &mut rich, GlobalCell::new(9, 2, 7), 0),
            Err("you already have a camp")
        );
        assert_eq!(rich.count(items::STONE_CHUNK), 5);

        let other = SiteOwner::Slot(2);
        camps
            .found(other, &mut stones(4), GlobalCell::new(50, 2, 7), 0)
            .unwrap();
        assert_ne!(camps.camp(other).unwrap().site_id, camp.site_id);
        assert_eq!(camps.sites(me), vec![camp]);
    }
}
