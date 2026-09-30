#![cfg(feature = "client")]
//! Client-side cost of drawing a generated world: how many instances the debug
//! terrain renderer emits around the camera and how long a rebuild takes.
//!
//! Run: `cargo test -p sandbox --features client --release --test worldgen_render_cost -- --ignored --nocapture`.

use std::time::Instant;

use sandbox::worldgen_scene;
use spall_client::window::build_instances;

/// Camera positions in metres (flying views from the ENG-114 tour, plus the
/// spawn): `(name, [x, y, z])`.
const VIEWS: [(&str, [f64; 3]); 5] = [
    ("spawn", [128.1, 41.5, 128.1]),
    ("overview", [200.0, 110.0, 235.0]),
    ("swamp", [60.0, 50.0, 236.0]),
    ("desert", [200.0, 58.0, 200.0]),
    ("mountains", [128.0, 70.0, 60.0]),
];

#[test]
#[ignore]
fn measure_terrain_rebuild_on_the_showcase() {
    let scene = worldgen_scene::generate("showcase", 1, worldgen_scene::DEFAULT_SIZE_CELLS)
        .expect("generate");
    let volume = &scene.world().terrain;
    for (name, center) in VIEWS {
        let start = Instant::now();
        let instances = build_instances(volume, center);
        println!(
            "{name:10} build_instances {:>8.1} ms, {:>8} instances",
            start.elapsed().as_secs_f64() * 1e3,
            instances.len()
        );
    }
}
