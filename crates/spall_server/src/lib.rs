//! GPU-free authoritative-server host.
//!
//! [`run`] is the T00 bounded process (no world state, no transport) still used
//! by `cargo xtask smoke`. [`serve`] is the T10 networked replication host: it
//! wraps a [`spall_sim::Simulation`] behind a [`spall_net`] QUIC endpoint,
//! accepts clients, and broadcasts committed topology transactions plus 20 Hz
//! motion snapshots.

pub mod baseline;
pub mod persist;
pub mod persist_pipeline;
pub mod serve;

pub use baseline::{
    BaselineError, BaselineTransfer, brick_repair_patch, capture_transfer, chunk_payload,
    transfer_from_world, world_baseline,
};
pub use persist::{
    CrashSuiteReport, PersistConfig, PersistError, RecoveryChoice, ScenarioResult, capture,
    journal_records, pose_batch_record, replay_from_base, replay_from_base_builtin, restore,
    run_crash_suite,
};
pub use persist_pipeline::{
    DEFAULT_QUEUE_CAPACITY, PersistPipeline, PipelineConfig, PipelineError, PipelineOutcome,
    PipelineStatus,
};
pub use serve::{Scene, ServeConfig, ServeError, ServeSummary, serve};

use spall_core::{JsonlError, JsonlLog, ProcessEvent, ProcessRecord, ProcessRole};
use std::{net::SocketAddr, path::PathBuf, thread, time::Duration};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub world: PathBuf,
    pub seed: u64,
    pub listen: SocketAddr,
    pub ticks: u64,
    pub log_json: PathBuf,
    pub ready_delay: Duration,
    pub fail_after_tick: Option<u64>,
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Log(#[from] JsonlError),
    #[error("requested test failure at tick {0}")]
    RequestedFailure(u64),
}

/// Runs a bounded, deliberately GPU-free tick loop. It owns no game state until T08.
pub fn run(config: &ServerConfig) -> Result<(), ServerError> {
    let mut log = JsonlLog::create(&config.log_json)?;
    log.write(&ProcessRecord::new(
        ProcessEvent::Started,
        ProcessRole::Server,
        Some(format!(
            "requested_listen={}, seed={}, world={}; transport_unimplemented=T09",
            config.listen,
            config.seed,
            config.world.display()
        )),
    ))?;
    if !config.ready_delay.is_zero() {
        thread::sleep(config.ready_delay);
    }
    log.write(&ProcessRecord::new(
        ProcessEvent::Ready,
        ProcessRole::Server,
        None,
    ))?;
    for tick in 0..config.ticks {
        if config.fail_after_tick == Some(tick) {
            log.write(&ProcessRecord::new(
                ProcessEvent::Failed,
                ProcessRole::Server,
                Some(format!("requested failure at tick {tick}")),
            ))?;
            return Err(ServerError::RequestedFailure(tick));
        }
    }
    log.write(&ProcessRecord::new(
        ProcessEvent::Stopped,
        ProcessRole::Server,
        Some(format!("completed_ticks={}", config.ticks)),
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{ProcessRole, log_has_ready_record};

    fn config(name: &str) -> ServerConfig {
        ServerConfig {
            world: PathBuf::from(".local/test-world"),
            seed: 42,
            listen: "127.0.0.1:5000".parse().unwrap(),
            ticks: 60,
            log_json: std::env::temp_dir()
                .join(format!("spall-server-{}-{name}.jsonl", std::process::id())),
            ready_delay: Duration::ZERO,
            fail_after_tick: None,
        }
    }

    #[test]
    fn bounded_server_writes_ready_and_stopped_records() {
        let config = config("bounded");
        run(&config).unwrap();
        assert!(
            log_has_ready_record(&config.log_json, ProcessRole::Server, std::process::id())
                .unwrap()
        );
        let log = std::fs::read_to_string(&config.log_json).unwrap();
        assert!(log.contains("completed_ticks=60"));
        std::fs::remove_file(config.log_json).unwrap();
    }

    #[test]
    fn requested_failure_is_reported_in_jsonl() {
        let mut config = config("failure");
        config.fail_after_tick = Some(3);
        assert!(matches!(
            run(&config),
            Err(ServerError::RequestedFailure(3))
        ));
        assert!(
            std::fs::read_to_string(&config.log_json)
                .unwrap()
                .contains("requested failure at tick 3")
        );
        std::fs::remove_file(config.log_json).unwrap();
    }
}
