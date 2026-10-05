use spall_protocol::component_water::{AmountCodecError, ComponentAmount, ComponentAmounts};

#[test]
fn exact_geometry_identity_changes_with_membership_or_grid_even_at_equal_open_count() {
    use spall_protocol::component_water::geometry_hash;
    let hash = |mask: &[u8], origin, coarsen| {
        geometry_hash(origin, [3, 3, 3], coarsen, 0.25_f64.to_bits(), mask).unwrap()
    };
    let mut a = vec![1; 27];
    a[1] = 0;
    let mut b = a.clone();
    b[1] = 1;
    b[2] = 0;
    assert_ne!(hash(&a, [0; 3], 3), hash(&b, [0; 3], 3));
    assert_ne!(hash(&a, [0; 3], 3), hash(&a, [-3, 0, 0], 3));
    assert_ne!(hash(&a, [0; 3], 3), hash(&a, [0; 3], 1));
    assert!(geometry_hash([0; 3], [3; 3], 3, 0.25_f64.to_bits(), &a[..26]).is_err());
    a[0] = 2;
    assert!(geometry_hash([0; 3], [3; 3], 3, 0.25_f64.to_bits(), &a).is_err());
}

fn state() -> ComponentAmounts {
    ComponentAmounts {
        origin: [-3, 0, 0],
        dimensions: [3, 3, 3],
        coarsen: 3,
        voxel_size_m_bits: 0.25_f64.to_bits(),
        geometry_hash: [17; 32],
        components: vec![
            ComponentAmount {
                anchor: [-3, 0, 0],
                open_voxels: 9,
                water_m3_bits: 0.125_f64.to_bits(),
                trapped_m3_bits: 0.5_f64.to_bits(),
            },
            ComponentAmount {
                anchor: [-1, 0, 0],
                open_voxels: 9,
                water_m3_bits: 0.0_f64.to_bits(),
                trapped_m3_bits: 0.0_f64.to_bits(),
            },
        ],
    }
}

#[test]
fn same_coarse_total_retains_distinct_wall_sides_and_trapped_amounts() {
    let left = state();
    let mut right = left.clone();
    right.components[0].water_m3_bits = 0.0_f64.to_bits();
    right.components[1].water_m3_bits = 0.125_f64.to_bits();
    let bytes = left.encode().unwrap();
    assert_eq!(&bytes[..6], b"SCWA\x01\x00");
    assert_eq!(&bytes[6..14], &(-3_i64).to_le_bytes());
    assert_eq!(bytes.len(), 90 + 2 * 44);
    assert_ne!(bytes, right.encode().unwrap());
    assert_eq!(
        ComponentAmounts::decode(&bytes, left.geometry_hash).unwrap(),
        left
    );
    assert_eq!(
        ComponentAmounts::decode(&right.encode().unwrap(), right.geometry_hash).unwrap(),
        right
    );
}

#[test]
fn decode_rejects_all_truncations_unknown_versions_count_overflow_and_geometry_mismatch() {
    let s = state();
    let bytes = s.encode().unwrap();
    for end in 0..bytes.len() {
        assert!(ComponentAmounts::decode(&bytes[..end], s.geometry_hash).is_err());
    }
    assert_eq!(
        ComponentAmounts::decode(&bytes, [18; 32]),
        Err(AmountCodecError::Geometry)
    );
    let mut bad = bytes.clone();
    bad[4] = 2;
    assert!(ComponentAmounts::decode(&bad, s.geometry_hash).is_err());
    let mut bad = bytes.clone();
    bad[86..90].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        ComponentAmounts::decode(&bad, s.geometry_hash),
        Err(AmountCodecError::Limit)
    );
    let mut bad = bytes;
    bad.push(0);
    assert!(ComponentAmounts::decode(&bad, s.geometry_hash).is_err());
}

#[test]
fn malformed_keys_capacity_and_amounts_cannot_be_encoded_or_decoded() {
    let base = state();
    for bits in [
        f64::NAN.to_bits(),
        f64::INFINITY.to_bits(),
        (-1.0_f64).to_bits(),
        1.0_f64.to_bits(),
    ] {
        let mut bad = base.clone();
        bad.components[0].water_m3_bits = bits;
        assert!(bad.encode().is_err());
    }
    let mut bad = base.clone();
    bad.components[1].anchor = bad.components[0].anchor;
    assert!(bad.encode().is_err());
    let mut bad = base.clone();
    bad.components.reverse();
    assert!(bad.encode().is_err());
    let mut bad = base.clone();
    bad.components[1].anchor[0] = 0;
    assert!(bad.encode().is_err());
    let mut bad = base.clone();
    bad.components[0].open_voxels = 28;
    assert!(bad.encode().is_err());
    let mut bad = base.clone();
    bad.components[0].trapped_m3_bits = f64::NAN.to_bits();
    assert!(bad.encode().is_err());
    let mut bytes = base.encode().unwrap();
    bytes[118..126].copy_from_slice(&f64::NAN.to_bits().to_le_bytes());
    assert!(ComponentAmounts::decode(&bytes, base.geometry_hash).is_err());
}
