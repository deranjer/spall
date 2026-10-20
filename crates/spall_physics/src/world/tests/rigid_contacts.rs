use super::*;

/// A prebuilt exclusion names bodies, not collider handles, so it keeps excluding a body whose
/// collider is rebuilt (an edit gives the collider a new handle).
#[test]
fn an_exclusion_still_hides_a_body_after_its_collider_is_rebuilt() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let floor_volume = crate::fixtures::floor_slab(VolumeId::new(11).unwrap(), 2, 2, 4);
    let grid = OccupancyGrid::from_volume(&floor_volume).unwrap().unwrap();
    let floor = world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: grid.clone(),
        cell_m: CELL_M as f32,
        density_kg_m3: 1_000.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });
    world.step();
    let params = crate::character::CharacterParams::DEFAULT;
    // Standing above the floor and asked to move straight down by a metre.
    let sweep = |world: &mut PhysicsWorld, exclusion: &ColliderExclusion| {
        world
            .sweep_character_pushing_excluding_set(
                params,
                [0.5, 1.5, 0.5],
                [0.0, -1.0, 0.0],
                1.0 / 60.0,
                exclusion,
                &[],
            )
            .translation_m[1]
    };
    let none = ColliderExclusion::default();
    let hidden = world.exclusion_for([floor]);
    let blocked = sweep(&mut world, &none);
    let through = sweep(&mut world, &hidden);
    assert!(blocked > -1.0, "the floor stops the character: {blocked}");
    assert!(
        through < blocked - 0.05,
        "excluded, it falls through: {through} vs {blocked}"
    );

    world.rebuild_collider(floor, &grid, Representation::MergedCuboids);
    world.step();
    let after_blocked = sweep(&mut world, &none);
    let after_through = sweep(&mut world, &hidden);
    assert!(
        (after_blocked - blocked).abs() < 1.0e-3,
        "the rebuilt floor still blocks"
    );
    assert!(
        (after_through - through).abs() < 1.0e-3,
        "the same exclusion still hides the rebuilt collider: {after_through} vs {through}"
    );
}

#[test]
fn character_push_uses_fine_voxel_mass() {
    let (small_speed, small_mass) = pushed_cube_speed(20.0);
    let (large_speed, large_mass) = pushed_cube_speed(2_000.0);
    assert!(
        (small_mass - 20.0).abs() < 1.0e-3,
        "small mass={small_mass}"
    );
    assert!(
        (large_mass - 2_000.0).abs() < 1.0e-2,
        "large mass={large_mass}"
    );
    assert!(small_speed > 0.0, "the player did not push the small cube");
    assert!(
        small_speed > large_speed * 5.0,
        "voxel mass should resist the same push: small={small_speed}, large={large_speed}"
    );
}

#[test]
fn high_restitution_body_visibly_rebounds_from_zero_restitution_terrain() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let floor_volume = crate::fixtures::floor_slab(VolumeId::new(11).unwrap(), 2, 2, 4);
    let floor_grid = OccupancyGrid::from_volume(&floor_volume).unwrap().unwrap();
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: floor_grid,
        cell_m: CELL_M as f32,
        density_kg_m3: 1_000.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });

    let id = VolumeId::new(12).unwrap();
    let mut cube = Volume::new(id, CellSizeCode::Quarter);
    cube.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(1, 1, 1),
        fixtures::STONE,
    ))
    .unwrap();
    let grid = OccupancyGrid::from_volume(&cube).unwrap().unwrap();
    let props = analytic_mass_properties(&grid, CELL_M, |_| 2_000.0).to_body_properties();
    let body = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::NativeVoxels,
        grid,
        cell_m: CELL_M as f32,
        density_kg_m3: 2_000.0,
        mass_properties: Some(props),
        translation_m: [8.0, 5.0, 8.0],
        linvel_m_s: [0.0; 3],
    });
    world.set_restitution(body, 0.72);

    let mut fell = false;
    let mut rebound_speed = 0.0_f32;
    for _ in 0..180 {
        world.step();
        let vy = world.body_state(body).linvel_m_s[1];
        fell |= vy < -1.0;
        if fell {
            rebound_speed = rebound_speed.max(vy);
        }
    }
    assert!(
        rebound_speed > 1.0,
        "expected a visible rebound, max upward speed was {rebound_speed} m/s"
    );
}

#[test]
fn installed_fine_grid_properties_reach_the_solver() {
    let grid = mixed_density_grid();
    let analytic = analytic_mass_properties(&grid, CELL_M, mix_density);
    let mut world = gravity_free();
    let id = add_mixed_body(&mut world, Some(analytic.to_body_properties()));
    world.step();

    for (label, (mass, com, inertia)) in [
        ("adapter", world.derived_mass_properties(id)),
        ("live rapier body", world.live_body_mass_properties(id)),
    ] {
        let mass_rel = ((analytic.mass_kg - f64::from(mass)) / analytic.mass_kg).abs();
        assert!(
            mass_rel < 1e-4,
            "{label}: mass {mass} vs {}",
            analytic.mass_kg
        );

        // COM is pulled off the geometric centre (1.0 m) to x = 1.25 m.
        assert!(
            (f64::from(com[0]) - analytic.com_m[0]).abs() < 2e-4,
            "{label}: com_x {} vs {}",
            com[0],
            analytic.com_m[0]
        );
        assert!(
            com[0] > 1.10 && com[0] < 1.40,
            "{label}: com_x {} not shifted toward the dense block",
            com[0]
        );
        for (axis, &c) in com.iter().enumerate().skip(1) {
            assert!(
                (f64::from(c) - analytic.com_m[axis]).abs() < 2e-4,
                "{label}: com[{axis}] {c} vs {}",
                analytic.com_m[axis]
            );
        }

        let mut got = [
            f64::from(inertia[0]),
            f64::from(inertia[1]),
            f64::from(inertia[2]),
        ];
        let mut want = analytic.principal_diagonal();
        got.sort_by(|a, b| a.partial_cmp(b).unwrap());
        want.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let scale = want.iter().cloned().fold(1e-9_f64, f64::max);
        for i in 0..3 {
            assert!(
                (got[i] - want[i]).abs() / scale < 2e-3,
                "{label}: principal inertia {got:?} vs {want:?}"
            );
        }
    }
}

#[test]
fn uniform_density_body_keeps_the_geometric_centre() {
    // The pre-fix behaviour: no explicit properties -> Rapier's COM is the
    // geometric centre for any uniform density, so the mixed-material shift
    // is lost.
    let mut world = gravity_free();
    let id = add_mixed_body(&mut world, None);
    world.step();
    let (_, com, _) = world.derived_mass_properties(id);
    assert!(
        (com[0] - 1.0).abs() < 1e-4,
        "uniform density COM should stay at the geometric centre, got {}",
        com[0]
    );
}

#[test]
fn torque_impulse_response_matches_the_analytic_inertia() {
    let grid = mixed_density_grid();
    let analytic = analytic_mass_properties(&grid, CELL_M, mix_density);
    let diag = analytic.principal_diagonal(); // tensor is diagonal for this fixture
    let mut world = gravity_free();
    let id = add_mixed_body(&mut world, Some(analytic.to_body_properties()));
    world.step();

    // A small torque impulse about each axis; free space, so Δω = I⁻¹·L.
    let l = [0.30_f32, 0.20, 0.25];
    world.apply_torque_impulse(id, l);
    world.step();
    let w = world.body_state(id).angvel_rad_s;
    for axis in 0..3 {
        let expected = f64::from(l[axis]) / diag[axis];
        let rel = ((f64::from(w[axis]) - expected) / expected).abs();
        assert!(
            rel < 5e-3,
            "axis {axis}: ω {} vs analytic I⁻¹·L {expected}",
            w[axis]
        );
    }
}

#[test]
fn off_centre_impulse_uses_the_shifted_centre_of_mass() {
    let grid = mixed_density_grid();
    let analytic = analytic_mass_properties(&grid, CELL_M, mix_density);
    let diag = analytic.principal_diagonal();
    let mut world = gravity_free();
    let id = add_mixed_body(&mut world, Some(analytic.to_body_properties()));
    world.step();

    // Impulse along +z applied on the x = 0 face at the COM height/depth.
    // Torque about y is (r × J)_y with r = point − com = (−com_x, 0, 0):
    //   (r × J)_y = −( r_z·J_x − r_x·J_z ) = r_x·J_z  →  −com_x·J_z ... in
    // Rapier's left-to-right cross this resolves to ω_y ≈ com_x·J_z / I_yy.
    let jz = 0.4_f32;
    let point = [0.0_f32, analytic.com_m[1] as f32, analytic.com_m[2] as f32];
    world.apply_impulse_at_point(id, [0.0, 0.0, jz], point);
    world.step();
    let w = world.body_state(id).angvel_rad_s;

    let with_true_com = analytic.com_m[0] * f64::from(jz) / diag[1];
    let with_geom_com = 1.0 * f64::from(jz) / diag[1];
    let err_true = (f64::from(w[1]).abs() - with_true_com.abs()).abs();
    let err_geom = (f64::from(w[1]).abs() - with_geom_com.abs()).abs();
    assert!(
        err_true < 0.1 * with_true_com.abs(),
        "ω_y {} should track the shifted COM prediction {with_true_com}",
        w[1]
    );
    assert!(
        err_true < err_geom,
        "ω_y {} fits the true COM (1.25 m) better than the geometric centre (1.0 m)",
        w[1]
    );
}
