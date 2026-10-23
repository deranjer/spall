//! The player's inventory as the pack (Tab) and the blueprint book (B) see
//! it: refreshed from the server over the progression stream while either
//! is open, plus the one craft request the book may have in flight.

use super::*;
use spall_protocol::{
    InventoryEntry, ProgressionOperation, ProgressionOutcome, ProgressionRejectCode,
    ProgressionRequest, ProgressionResponse,
};

/// How often an open pack or book asks the server for the inventory again.
const REFRESH: Duration = Duration::from_millis(1500);

/// What came of a craft the book asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CraftOutcome {
    Crafted { recipe_id: u16 },
    Rejected(ProgressionRejectCode),
}

#[derive(Debug, Clone, Copy)]
struct PendingCraft {
    request_id: u64,
    recipe_id: u16,
    /// A craft refused only because the inventory moved on (a harvest
    /// landed) is sent once more against the newer revision.
    retried: bool,
}

pub(super) struct InventoryFeed {
    /// `None` until the first answer since the feed was reset.
    stacks: Option<Vec<InventoryEntry>>,
    revision: u64,
    requested_at: Option<Instant>,
    next_request_id: u64,
    craft: Option<PendingCraft>,
}

impl InventoryFeed {
    pub(super) fn new() -> Self {
        // The server keeps a per-player high-water mark of request IDs, so
        // start each session above any earlier one: wall-clock milliseconds.
        let next_request_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |since| since.as_millis() as u64)
            .max(1);
        Self {
            stacks: None,
            revision: 0,
            requested_at: None,
            next_request_id,
            craft: None,
        }
    }

    /// Forgets what was known and asks again (a pack or book opening).
    pub(super) fn reset(&mut self) {
        self.stacks = None;
        self.requested_at = None;
    }

    pub(super) fn stacks(&self) -> Option<&[InventoryEntry]> {
        self.stacks.as_deref()
    }

    pub(super) fn count(&self, item_id: u16) -> u32 {
        self.stacks()
            .unwrap_or_default()
            .iter()
            .find(|entry| entry.item_id == item_id)
            .map_or(0, |entry| entry.count)
    }

    pub(super) fn crafting(&self) -> bool {
        self.craft.is_some()
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    /// Takes a progression answer. Returns the outcome when it answers the
    /// craft in flight, and a request to send when that craft is worth one
    /// more try.
    pub(super) fn take_reply(
        &mut self,
        reply: ProgressionResponse,
    ) -> (Option<CraftOutcome>, Option<ProgressionRequest>) {
        self.stacks = Some(reply.inventory);
        self.revision = reply.inventory_revision;
        let Some(craft) = self
            .craft
            .filter(|craft| craft.request_id == reply.request_id)
        else {
            return (None, None);
        };
        match reply.outcome {
            ProgressionOutcome::Crafted => {
                self.craft = None;
                (
                    Some(CraftOutcome::Crafted {
                        recipe_id: craft.recipe_id,
                    }),
                    None,
                )
            }
            ProgressionOutcome::Rejected(ProgressionRejectCode::InventoryRevision)
                if !craft.retried =>
            {
                let request = self.craft_request(craft.recipe_id, reply.catalog_version, true);
                (None, Some(request))
            }
            ProgressionOutcome::Rejected(code) => {
                self.craft = None;
                (Some(CraftOutcome::Rejected(code)), None)
            }
            ProgressionOutcome::Inventory => {
                self.craft = None;
                (
                    Some(CraftOutcome::Rejected(ProgressionRejectCode::Unavailable)),
                    None,
                )
            }
        }
    }

    /// The inventory request to send now, if one is due.
    pub(super) fn due_request(
        &mut self,
        catalog_version: u32,
        now: Instant,
    ) -> Option<ProgressionRequest> {
        if self
            .requested_at
            .is_some_and(|at| now.saturating_duration_since(at) < REFRESH)
        {
            return None;
        }
        self.requested_at = Some(now);
        Some(ProgressionRequest {
            request_id: self.next_id(),
            catalog_version,
            expected_inventory_revision: 0,
            operation: ProgressionOperation::InspectInventory,
        })
    }

    /// One batch of `recipe_id`, against the newest inventory revision.
    pub(super) fn craft(&mut self, recipe_id: u16, catalog_version: u32) -> ProgressionRequest {
        self.craft_request(recipe_id, catalog_version, false)
    }

    fn craft_request(
        &mut self,
        recipe_id: u16,
        catalog_version: u32,
        retried: bool,
    ) -> ProgressionRequest {
        let request_id = self.next_id();
        self.craft = Some(PendingCraft {
            request_id,
            recipe_id,
            retried,
        });
        ProgressionRequest {
            request_id,
            catalog_version,
            expected_inventory_revision: self.revision,
            operation: ProgressionOperation::Craft {
                recipe_id,
                batch_count: 1,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(request_id: u64, revision: u64, outcome: ProgressionOutcome) -> ProgressionResponse {
        ProgressionResponse {
            request_id,
            catalog_version: 1,
            inventory_revision: revision,
            outcome,
            inventory: vec![InventoryEntry {
                item_id: 1,
                count: 5,
            }],
        }
    }

    #[test]
    fn asks_once_per_refresh_and_keeps_the_newest_answer() {
        let mut feed = InventoryFeed::new();
        let now = Instant::now();
        let first = feed.due_request(1, now).expect("asks at once");
        assert_eq!(first.operation, ProgressionOperation::InspectInventory);
        assert_eq!(feed.due_request(1, now + Duration::from_millis(100)), None);
        let second = feed.due_request(1, now + REFRESH).expect("refresh");
        assert!(second.request_id > first.request_id);
        assert_eq!(feed.stacks(), None);
        feed.take_reply(reply(second.request_id, 3, ProgressionOutcome::Inventory));
        assert_eq!(feed.count(1), 5);
        assert_eq!(feed.count(2), 0);
        feed.reset();
        assert_eq!(feed.stacks(), None);
        assert!(
            feed.due_request(1, now + REFRESH).is_some(),
            "asks again after a reset"
        );
    }

    #[test]
    fn a_craft_is_sent_against_the_newest_revision_and_its_answer_reported() {
        let mut feed = InventoryFeed::new();
        feed.take_reply(reply(1, 7, ProgressionOutcome::Inventory));
        let craft = feed.craft(2, 1);
        assert_eq!(craft.expected_inventory_revision, 7);
        assert_eq!(
            craft.operation,
            ProgressionOperation::Craft {
                recipe_id: 2,
                batch_count: 1
            }
        );
        assert!(feed.crafting());
        // An unrelated inventory answer is not the craft's.
        assert_eq!(
            feed.take_reply(reply(
                craft.request_id + 50,
                7,
                ProgressionOutcome::Inventory
            )),
            (None, None)
        );
        assert_eq!(
            feed.take_reply(reply(craft.request_id, 8, ProgressionOutcome::Crafted)),
            (Some(CraftOutcome::Crafted { recipe_id: 2 }), None)
        );
        assert!(!feed.crafting());
    }

    #[test]
    fn a_craft_beaten_by_a_harvest_is_retried_once_then_reported() {
        let mut feed = InventoryFeed::new();
        let craft = feed.craft(1, 1);
        let stale = ProgressionOutcome::Rejected(ProgressionRejectCode::InventoryRevision);
        let (outcome, retry) = feed.take_reply(reply(craft.request_id, 9, stale));
        assert_eq!(outcome, None);
        let retry = retry.expect("one more try");
        assert_eq!(retry.expected_inventory_revision, 9);
        let (outcome, again) = feed.take_reply(reply(retry.request_id, 10, stale));
        assert_eq!(
            outcome,
            Some(CraftOutcome::Rejected(
                ProgressionRejectCode::InventoryRevision
            ))
        );
        assert_eq!(again, None);
        assert!(!feed.crafting());
    }
}
