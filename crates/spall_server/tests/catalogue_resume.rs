//! A world reset that interrupts a client's catalogue must not throw away what the client has
//! already verified. The server derives, from the catalogue layout and the number of chunks it
//! has written, exactly which digests the client holds; the client derives the same set from the
//! segments it completed. Both then agree on a delta that carries only the rest.

use std::collections::BTreeMap;

use spall_client::{CatalogueReceiver, ReplicaConfig, ReplicaWorld};
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_protocol::{Hash32, InterestEpoch, TransferId};
use spall_server::baseline::{
    BaselineTransfer, CatalogueBasis, snapshot_world, transfer_from_snapshot_deferred_resuming,
};
use spall_sim::{Simulation, SimulationConfig, fixtures};
use spall_voxel::{Brick, Volume};

/// Bricks per side of the test floor: 64 x 64 = 4,096 bricks, about 5 catalogue chunks.
const SIDE: i64 = 64;

/// A floor of `SIDE x SIDE` uniform bricks, each a different material so every brick has its own
/// content hash (as real terrain does), without the memory of dense bricks. `altered(i)` gives
/// brick `i` a different material, standing for terrain that drifted from the pristine world.
fn grid_server(altered: impl Fn(usize) -> bool) -> Simulation {
    let id = VolumeId::new(1).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for i in 0..(SIDE * SIDE) as usize {
        let coord = BrickCoord::new(i as i64 % SIDE, 0, i as i64 / SIDE);
        // The brick the terrain collider covers is plain stone; the others differ.
        let material = if i == 0 {
            fixtures::STONE
        } else {
            MaterialId(100 + i as u16 + if altered(i) { 20_000 } else { 0 })
        };
        volume
            .insert_brick(coord, Brick::uniform(material, Revision(1 + i as u64)))
            .unwrap();
    }
    let mut setup = fixtures::flat_terrain_setup();
    setup.terrain = volume;
    setup.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(31, 9, 31));
    setup.physics.disable_ccd = true;
    Simulation::new(SimulationConfig::new(setup)).unwrap()
}

fn deferred_transfer(
    sim: &Simulation,
    id: u64,
    resume: Option<(&CatalogueBasis, Hash32, Option<u32>)>,
) -> BaselineTransfer {
    transfer_from_snapshot_deferred_resuming(
        snapshot_world(sim, None).with_region([0.0; 3], 1),
        TransferId(id),
        InterestEpoch(1),
        resume,
    )
    .unwrap()
}

fn install_core(replica: &mut ReplicaWorld, transfer: &BaselineTransfer) {
    let mut receiver = spall_client::segmented::SegmentedReceiver::with_limits(
        transfer.begin.checkpoint_tick.get(),
        None,
        transfer.begin.total_bytes,
    );
    for part in transfer.parts.iter() {
        receiver.push(&part.payload).unwrap();
    }
    replica
        .install_staged(receiver.finish().unwrap().staged)
        .unwrap();
    replica.expect_catalogue(transfer.begin.transfer_id);
}

fn receiver_for(transfer: &BaselineTransfer) -> CatalogueReceiver {
    CatalogueReceiver::new(
        transfer.begin.transfer_id,
        transfer.begin.checkpoint_tick.get(),
        u64::MAX,
    )
}

/// Feeds every chunk of `transfer`'s catalogue and returns the staged digests.
fn receive_all(transfer: &BaselineTransfer) -> spall_client::replica::StagedBaseline {
    let mut receiver = receiver_for(transfer);
    let mut staged = None;
    for chunk in transfer.catalogue.as_ref().unwrap().chunks.iter() {
        staged = receiver.push(chunk).unwrap();
    }
    staged.expect("the last chunk completes the catalogue")
}

/// The first `chunks` chunks of `transfer`'s catalogue, as a client processes them.
fn receive_prefix(transfer: &BaselineTransfer, chunks: u32) -> CatalogueReceiver {
    let mut receiver = receiver_for(transfer);
    for chunk in transfer
        .catalogue
        .as_ref()
        .unwrap()
        .chunks
        .iter()
        .take(chunks as usize)
    {
        assert!(receiver.push(chunk).unwrap().is_none(), "still incomplete");
    }
    receiver
}

/// Every digest in `client` is in `server` with the same `(revision, content hash)`, and the two
/// hold the same number: the two ends agree on what the client holds.
fn assert_same_held(
    server: &CatalogueBasis,
    client: &BTreeMap<BrickCoord, spall_voxel::BrickDigest>,
) {
    assert_eq!(server.len(), client.len(), "the same number of digests");
    for (coord, digest) in client {
        assert_eq!(
            server.entry(*coord),
            Some((digest.revision.get(), digest.content_hash.to_bytes())),
            "{coord:?}"
        );
    }
}

fn new_replica() -> ReplicaWorld {
    ReplicaWorld::empty(ReplicaConfig::default())
}

#[test]
fn a_reset_mid_catalogue_keeps_what_was_verified_and_converges() {
    // The client joined a drifted world; a reset will replace it with the pristine one.
    let old = grid_server(|i| i % 11 == 0);
    let first = deferred_transfer(&old, 7, None);
    let catalogue = first.catalogue.as_ref().expect("catalogue");
    assert!(
        catalogue.chunks.len() >= 4,
        "the test world must span several chunks"
    );
    let layout = catalogue.layout.clone();
    let mut replica = new_replica();
    install_core(&mut replica, &first);

    // The reset arrives when the client has processed the first half of the chunks.
    let processed = (catalogue.chunks.len() / 2) as u32;
    let interrupted = receive_prefix(&first, processed);
    let resume = interrupted
        .resume_state(Some(BTreeMap::new()))
        .expect("progress to resume from");

    let new = grid_server(|_| false);
    let expected = new.world().world_hash();
    let held = layout.held_after(processed);
    assert!(!held.is_empty(), "some segments completed");
    assert!(
        held.len() < catalogue.bricks as usize,
        "but not the whole catalogue"
    );

    let full = deferred_transfer(&new, 8, None);
    let delta = deferred_transfer(&new, 9, Some((&held, expected, Some(processed))));
    let full_catalogue = full.catalogue.as_ref().unwrap();
    let delta_catalogue = delta.catalogue.as_ref().unwrap();
    assert!(!full_catalogue.delta && delta_catalogue.delta);
    assert!(
        delta
            .catalogue
            .as_ref()
            .unwrap()
            .chunks
            .iter()
            .all(|c| c.basis_chunks == Some(processed)),
        "every chunk names the prefix it builds on"
    );
    assert!(
        delta_catalogue.bricks < full_catalogue.bricks,
        "the delta carries less than a full catalogue ({} of {})",
        delta_catalogue.bricks,
        full_catalogue.bricks
    );
    assert!(delta_catalogue.payload_bytes() < full_catalogue.payload_bytes());

    // The client holds exactly the digests the server believes it holds.
    let basis = resume.held_after(processed).unwrap();
    assert_same_held(&held, &basis);

    // And merging the delta onto them reproduces the new world.
    install_core(&mut replica, &delta);
    let outcomes = replica
        .complete_delta_catalogue(receive_all(&delta), basis, expected)
        .unwrap();
    assert!(outcomes.is_empty());
    assert_eq!(replica.world_hash(), new.world().world_hash());
}

#[test]
fn a_second_reset_during_the_resumed_delta_still_converges() {
    let first_world = grid_server(|i| i % 11 == 0);
    let first = deferred_transfer(&first_world, 7, None);
    let mut replica = new_replica();
    install_core(&mut replica, &first);
    let chunks = first.catalogue.as_ref().unwrap().chunks.len() as u32;

    // Reset one arrives after half of the first catalogue.
    let k1 = chunks / 2;
    let resume1 = receive_prefix(&first, k1)
        .resume_state(Some(BTreeMap::new()))
        .unwrap();
    let second_world = grid_server(|i| i % 7 == 0);
    let held1 = first.catalogue.as_ref().unwrap().layout.held_after(k1);
    let second = deferred_transfer(
        &second_world,
        8,
        Some((&held1, second_world.world().world_hash(), Some(k1))),
    );
    install_core(&mut replica, &second);
    let basis1 = resume1.held_after(k1).unwrap();
    assert_same_held(&held1, &basis1);

    // Reset two interrupts that delta, part-way. What the client holds is its previous basis
    // plus the delta's completed segments, on both ends.
    let delta_chunks = second.catalogue.as_ref().unwrap().chunks.len() as u32;
    assert!(delta_chunks >= 2, "the delta spans several chunks");
    let k2 = delta_chunks / 2;
    let interrupted = receive_prefix(&second, k2);
    assert!(interrupted.delta());
    assert_eq!(interrupted.basis_chunks(), Some(k1));
    let resume2 = interrupted
        .resume_state(Some(basis1))
        .expect("an interrupted delta resumes from its own basis");
    let held2 = second.catalogue.as_ref().unwrap().layout.held_after(k2);

    let third_world = grid_server(|_| false);
    let expected = third_world.world().world_hash();
    let third = deferred_transfer(&third_world, 9, Some((&held2, expected, Some(k2))));
    assert!(third.catalogue.as_ref().unwrap().delta);
    install_core(&mut replica, &third);
    let basis2 = resume2.held_after(k2).unwrap();
    assert_same_held(&held2, &basis2);
    replica
        .complete_delta_catalogue(receive_all(&third), basis2, expected)
        .unwrap();
    assert_eq!(replica.world_hash(), third_world.world().world_hash());
}

#[test]
fn a_catalogue_with_nothing_verified_yet_is_resent_in_full() {
    let world = grid_server(|_| false);
    let first = deferred_transfer(&world, 7, None);
    let layout = first.catalogue.as_ref().unwrap().layout.clone();
    // No chunk processed: there is nothing to resume from, on either end.
    assert!(layout.held_after(0).is_empty());
    assert!(
        receiver_for(&first)
            .resume_state(Some(BTreeMap::new()))
            .is_none()
    );
}

#[test]
fn a_fully_received_layout_holds_every_digest() {
    let world = grid_server(|_| false);
    let first = deferred_transfer(&world, 7, None);
    let catalogue = first.catalogue.as_ref().unwrap();
    let all = catalogue.layout.held_after(catalogue.chunks.len() as u32);
    assert_eq!(all.len() as u64, catalogue.bricks);
}

#[test]
fn the_server_cannot_resume_from_more_than_the_client_processed() {
    let world = grid_server(|_| false);
    let first = deferred_transfer(&world, 7, None);
    let chunks = first.catalogue.as_ref().unwrap().chunks.len() as u32;
    let resume = receive_prefix(&first, 2)
        .resume_state(Some(BTreeMap::new()))
        .unwrap();
    assert!(resume.held_after(2).is_ok());
    assert!(
        resume.held_after(3).is_err(),
        "a basis beyond what was processed is refused, loudly"
    );
    assert!(resume.held_after(chunks).is_err());
}

#[test]
fn a_new_world_missing_a_held_brick_falls_back_to_a_full_catalogue() {
    let old = grid_server(|_| false);
    let first = deferred_transfer(&old, 7, None);
    let layout = first.catalogue.as_ref().unwrap().layout.clone();
    let processed = 2;
    // The client somehow holds a brick the new world does not have.
    let mut entries = std::collections::HashMap::new();
    entries.insert(BrickCoord::new(9_000, 0, 9_000), (1, [7u8; 32]));
    let stray = CatalogueBasis::from_entries(entries);
    let new = grid_server(|_| false);
    let expected = new.world().world_hash();
    let transfer = deferred_transfer(&new, 8, Some((&stray, expected, Some(processed))));
    let catalogue = transfer.catalogue.as_ref().unwrap();
    assert!(!catalogue.delta, "a delta cannot remove a brick");
    assert!(
        catalogue
            .chunks
            .iter()
            .all(|c| c.basis_chunks.is_none() && !c.delta),
        "and a full catalogue names no basis"
    );
    // The honest prefix, in contrast, is accepted.
    let honest = layout.held_after(processed);
    let transfer = deferred_transfer(&new, 9, Some((&honest, expected, Some(processed))));
    assert!(transfer.catalogue.as_ref().unwrap().delta);
}

#[test]
fn a_client_ahead_of_the_servers_count_trims_to_the_prefix_the_server_assumed() {
    // The server computes its delta when it has written `assumed` chunks, but more are written
    // before the reset's baseline reaches the wire, so the client has processed `processed`.
    let old = grid_server(|i| i % 5 == 0);
    let first = deferred_transfer(&old, 7, None);
    let layout = first.catalogue.as_ref().unwrap().layout.clone();
    let (assumed, processed) = (2u32, 4u32);
    let resume = receive_prefix(&first, processed)
        .resume_state(Some(BTreeMap::new()))
        .unwrap();
    let client_basis = resume.held_after(assumed).unwrap();
    assert_same_held(&layout.held_after(assumed), &client_basis);
    assert!(
        resume.held_after(processed).unwrap().len() > client_basis.len(),
        "the client really did hold more, and ignores it"
    );

    let new = grid_server(|_| false);
    let expected = new.world().world_hash();
    let delta = deferred_transfer(
        &new,
        8,
        Some((&layout.held_after(assumed), expected, Some(assumed))),
    );
    let mut replica = new_replica();
    install_core(&mut replica, &first);
    install_core(&mut replica, &delta);
    replica
        .complete_delta_catalogue(receive_all(&delta), client_basis, expected)
        .unwrap();
    assert_eq!(replica.world_hash(), new.world().world_hash());
}
