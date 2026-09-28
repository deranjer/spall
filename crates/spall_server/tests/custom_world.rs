//! `Scene::Custom` is host-supplied: serving it without a world must fail at
//! configuration time, not panic later on the simulation thread.

use spall_net::JoinToken;
use spall_server::{CustomWorld, Scene, ServeConfig, ServeError, serve};

fn config(dir: &std::path::Path, scene: Scene) -> ServeConfig {
    let mut cfg = ServeConfig::headless(
        "127.0.0.1:0".parse().unwrap(),
        scene,
        JoinToken::generate().unwrap(),
    );
    cfg.log_json = dir.join("server.jsonl");
    cfg
}

#[test]
fn custom_scene_without_a_world_is_a_configuration_error() {
    let dir = std::env::temp_dir().join(format!("spall-custom-world-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let error = serve(config(&dir, Scene::Custom)).unwrap_err();
    assert!(matches!(error, ServeError::Configuration(_)), "{error:?}");

    // A world with no spawn point is equally unusable: every client needs one.
    let mut cfg = config(&dir, Scene::Custom);
    cfg.custom_world = Some(CustomWorld::new(Vec::new(), || {
        spall_sim::fixtures::walk_arena_setup()
    }));
    let error = serve(cfg).unwrap_err();
    assert!(matches!(error, ServeError::Configuration(_)), "{error:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
