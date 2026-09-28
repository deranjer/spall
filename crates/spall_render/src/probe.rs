//! ENG-60 diagnostic instrumentation. `ScenePipeline::new` builds six GPU
//! pipelines (three T13/T14 compute pipelines via `IndirectPipeline::new`,
//! then the T12 opaque/shadow/tone-map render pipelines) and the pinned
//! Vulkan backend crashes natively (`STATUS_ACCESS_VIOLATION`) somewhere in
//! that sequence on the recorded NVIDIA driver — see `docs/reports/ENG-60.md`.
//! A hard native crash discards buffered stdio and unwinds nothing, so the
//! only way to know exactly which of the six calls faulted is a synced,
//! fsync'd marker written *before* each one.
//!
//! [`mark`] is a no-op unless `SPALL_PROBE_LOG` is set (mirrors
//! `examples/vulkan_shadow_probe.rs`'s own marker), so it costs one env-var
//! lookup on the pipeline-creation path used only at renderer start-up and is
//! otherwise invisible to normal rendering.

use std::io::Write;

/// Append `step` to the file named by `SPALL_PROBE_LOG`, flushed and
/// `fsync`'d so it survives a native crash on the next line. Does nothing if
/// the env var is unset (the normal, non-diagnostic case).
pub(crate) fn mark(step: &str) {
    let Ok(path) = std::env::var("SPALL_PROBE_LOG") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{step}");
        let _ = f.flush();
        let _ = f.sync_all();
    }
}
