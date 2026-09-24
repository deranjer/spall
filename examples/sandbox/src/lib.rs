//! Game-owned configuration belongs here.

pub mod content;
/// Minimal game content (material catalog, player tools) that turns tool use
/// into engine [`spall_sim::EditIntent`]s. The engine never imports this.
pub mod game;
pub mod progression_store;

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .try_init();
}
