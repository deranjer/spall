//! Game-owned configuration belongs here.

pub mod appearance;
pub mod appearance_extensions;
pub mod appearance_v3;
pub mod content;
pub mod editor_scene;
/// Minimal game content (material catalog, player tools) that turns tool use
/// into engine [`spall_sim::EditIntent`]s. The engine never imports this.
pub mod game;
pub mod progression_store;
pub mod worldgen_scene;

pub fn init_tracing() {
    // Harness-spawned children lose stderr, so `SPALL_LOG_FILE=<prefix>` sends
    // each process's tracing output to `<prefix>.<pid>.log` instead.
    if let Some(prefix) = std::env::var_os("SPALL_LOG_FILE") {
        let mut path = prefix;
        path.push(format!(".{}.log", std::process::id()));
        match std::fs::File::create(&path) {
            Ok(file) => {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
                    .try_init();
                return;
            }
            Err(error) => eprintln!("SPALL_LOG_FILE {path:?} unusable: {error}"),
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .try_init();
}

#[cfg(feature = "client")]
pub mod ecology_scene;

pub mod vegetation;
