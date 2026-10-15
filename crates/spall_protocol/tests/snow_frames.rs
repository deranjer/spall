use spall_protocol::{Record, SnowFootprint, SnowSurface, SnowSurfaceFrame, SnowSurfaceId};

fn layered_tile() -> SnowSurfaceFrame {
    let surfaces = (0..256)
        .map(|face| {
            let mut mask = [0u64; 4];
            mask[face / 64] = 1 << (face % 64);
            let mut id = [0; 16];
            id[14..].copy_from_slice(&(face as u16 + 1).to_be_bytes());
            SnowSurface {
                id: SnowSurfaceId(id),
                top_cell_y: 11,
                geometry_revision: 4,
                exposure_generation: face as u32 + 1,
                exposed_faces: mask,
                depth_q16: 8192,
            }
        })
        .collect::<Vec<_>>();
    let footprints = [0, 63, 64, 255]
        .into_iter()
        .enumerate()
        .map(|(i, face)| SnowFootprint {
            id: i as u64 + 1,
            surface_id: surfaces[face].id,
            geometry_revision: 4,
            contact_cell: [-16 + (face % 16) as i32, 11, -32 + (face / 16) as i32],
            offset_mm: [0, 249],
            heading_q15: i16::MIN,
            foot: (i % 2) as u8,
            created_tick: 100,
            strength_q8: 255,
        })
        .collect();
    SnowSurfaceFrame {
        schema: 1,
        world_epoch: 1,
        world_revision: 2,
        effective_tick: 100,
        snapshot_seq: 1,
        tile_x: -1,
        tile_z: -2,
        top_cell_y: 11,
        surfaces,
        footprints,
    }
}

#[test]
fn worst_partitioned_tile_is_bounded_and_negative_edge_contacts_round_trip() {
    let frame = layered_tile();
    let bytes = spall_protocol::encode_control(&frame).unwrap();
    assert!(bytes.len() < 64 * 1024);
    assert_eq!(
        spall_protocol::decode_control::<SnowSurfaceFrame>(&bytes).unwrap(),
        frame
    );
}

#[test]
fn tracks_cannot_claim_adjacent_tiles_other_layers_or_foreign_snow() {
    let frame = layered_tile();
    let mut outside = frame.clone();
    outside.footprints[0].contact_cell[0] = 0;
    assert!(outside.validate().is_err());
    let mut roof = frame.clone();
    roof.footprints[0].contact_cell[1] += 1;
    assert!(roof.validate().is_err());
    let mut other = frame.clone();
    other.footprints[0].surface_id = frame.surfaces[1].id;
    assert!(other.validate().is_err());
    let mut overlap = frame.clone();
    overlap.surfaces[1].exposed_faces = overlap.surfaces[0].exposed_faces;
    assert!(overlap.validate().is_err());
    let mut shallow = frame;
    shallow.surfaces[0].depth_q16 = 131;
    assert!(shallow.validate().is_err());
}

#[test]
fn expired_or_future_tracks_are_rejected_but_empty_layer_tombstones_are_valid() {
    let frame = layered_tile();
    let mut future = frame.clone();
    future.footprints[0].created_tick = 101;
    assert!(future.validate().is_err());
    let mut expired = frame.clone();
    expired.effective_tick = 18_101;
    assert!(expired.validate().is_err());
    let mut cleared = frame;
    cleared.surfaces.clear();
    cleared.footprints.clear();
    assert!(cleared.validate().is_ok());
}
