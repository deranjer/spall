//! `cargo xtask net-check` — the T09 transport harness.
//!
//! Runs one in-process QUIC server, N authenticated headless clients (each
//! behind its own opaque UDP loss proxy unless `--no-proxy`), exchanges every
//! channel, and tears everything down under a hard deadline. Writes the
//! standard evidence files (`summary.json`, `net.jsonl`, `metrics.json`).
//!
//! Separate-process bots that drive the *real* `sandbox-server` protocol arrive
//! with T10, when the server host speaks it; this command exercises the
//! `spall_net` transport itself.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use serde::Serialize;
use spall_core::{JsonlLog, ProcessEvent, ProcessRecord, ProcessRole};
use spall_net::harness::{TransportCheckParams, run_transport_check};
use spall_net::{PacketFaultPlan, TransportConfig};

use crate::XtaskError;

#[derive(Debug, Args)]
pub struct NetCheckArgs {
    /// Headless clients to connect.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=32))]
    clients: u64,
    /// Reliable control records each client sends and expects echoed.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(0..=10_000))]
    records: u64,
    /// Motion/input datagrams each client sends.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(0..=10_000))]
    datagrams: u64,
    /// Encrypted-packet loss applied by the proxy, in percent (both directions).
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(0..=90))]
    loss_percent: u8,
    /// Deterministic proxy seed.
    #[arg(long, default_value_t = 0x5A11_0000_0000_0009)]
    seed: u64,
    /// Skip the UDP proxy and connect straight to the server.
    #[arg(long)]
    no_proxy: bool,
    /// Whole-run deadline in milliseconds.
    #[arg(long, default_value_t = 30_000, value_parser = clap::value_parser!(u64).range(1_000..=600_000))]
    timeout_ms: u64,
    /// Output directory. A unique `.local/runs` directory is created if omitted.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Serialize)]
struct NetSummary<'a> {
    version: u32,
    result: &'a str,
    platform: &'a str,
    clients: usize,
    clients_connected: usize,
    proxy: bool,
    loss_percent: u8,
    control_records_expected: u64,
    control_records_echoed: u64,
    bulk_parts_delivered: u64,
    datagrams_delivered: u64,
    dedup_dropped: u64,
    packets_forwarded: u64,
    packets_dropped: u64,
    transport_bytes_sent: u64,
    transport_bytes_recv: u64,
    note: &'a str,
}

pub fn run(args: NetCheckArgs, unique_output: impl FnOnce() -> PathBuf) -> Result<(), XtaskError> {
    let output = args.output.clone().unwrap_or_else(unique_output);
    std::fs::create_dir_all(&output).map_err(|source| XtaskError::Output {
        path: output.display().to_string(),
        source,
    })?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|source| XtaskError::Output {
            path: "tokio runtime".into(),
            source,
        })?;

    let mut log = JsonlLog::create(&output.join("net.jsonl"))?;
    log.write(&ProcessRecord::new(
        ProcessEvent::Started,
        ProcessRole::Server,
        Some(format!(
            "in-process transport check: clients={}, proxy={}, loss={}%",
            args.clients, !args.no_proxy, args.loss_percent
        )),
    ))?;

    let params = TransportCheckParams {
        clients: args.clients as usize,
        records_per_client: args.records as usize,
        datagrams_per_client: args.datagrams as usize,
        proxy: if args.no_proxy {
            None
        } else {
            Some(PacketFaultPlan {
                seed: args.seed,
                loss_ratio: args.loss_percent as f64 / 100.0,
                duplicate_ratio: 0.0,
                delay: Duration::from_millis(15),
                jitter: Duration::from_millis(10),
            })
        },
        run_bulk_transfer: true,
        config: TransportConfig::default(),
        overall_timeout: Duration::from_millis(args.timeout_ms),
    };

    let report = match runtime.block_on(run_transport_check(params.clone())) {
        Ok(report) => report,
        Err(error) => {
            log.write(&ProcessRecord::new(
                ProcessEvent::Failed,
                ProcessRole::Server,
                Some(error.to_string()),
            ))?;
            write_summary(&output, &failed_summary(&params, &error.to_string()))?;
            return Err(XtaskError::Capability(format!(
                "transport check did not complete: {error}"
            )));
        }
    };

    log.write(&ProcessRecord::new(
        ProcessEvent::Ready,
        ProcessRole::Server,
        Some(format!(
            "{} clients authenticated",
            report.clients_connected
        )),
    ))?;
    for outcome in &report.clients {
        log.write(&ProcessRecord::new(
            ProcessEvent::Ready,
            ProcessRole::Client,
            Some(format!(
                "{}: records {}/{}, datagrams {}, bulk parts {}",
                outcome.session,
                outcome.records_recv,
                params.records_per_client,
                outcome.datagrams_recv,
                outcome.bulk_parts_recv
            )),
        ))?;
    }

    let packets_forwarded: u64 = report
        .proxy_stats
        .iter()
        .map(|s| s.c2s_forwarded + s.s2c_forwarded)
        .sum();
    let packets_dropped: u64 = report
        .proxy_stats
        .iter()
        .map(|s| s.c2s_dropped + s.s2c_dropped)
        .sum();

    let ok = report.all_reliable_delivered(&params);
    let summary = NetSummary {
        version: 1,
        result: if ok { "passed" } else { "failed" },
        platform: std::env::consts::OS,
        clients: params.clients,
        clients_connected: report.clients_connected,
        proxy: !args.no_proxy,
        loss_percent: args.loss_percent,
        control_records_expected: (params.clients * params.records_per_client) as u64,
        control_records_echoed: report.control_records_echoed,
        bulk_parts_delivered: report.bulk_parts_delivered,
        datagrams_delivered: report.datagrams_delivered,
        dedup_dropped: report.dedup_dropped,
        packets_forwarded,
        packets_dropped,
        transport_bytes_sent: report.transport_bytes_sent,
        transport_bytes_recv: report.transport_bytes_recv,
        note: "in-process QUIC transport check; separate-process protocol bots land with T10",
    };
    write_summary(&output, &summary)?;
    write_metrics(&output, &report)?;

    log.write(&ProcessRecord::new(
        ProcessEvent::Stopped,
        ProcessRole::Server,
        Some(format!("result={}", summary.result)),
    ))?;

    if ok {
        println!("net-check passed: {}", output.display());
        Ok(())
    } else {
        Err(XtaskError::Cargo(vec!["net-check".into()], 1))
    }
}

fn failed_summary<'a>(params: &TransportCheckParams, note: &'a str) -> NetSummary<'a> {
    NetSummary {
        version: 1,
        result: "failed",
        platform: std::env::consts::OS,
        clients: params.clients,
        clients_connected: 0,
        proxy: params.proxy.is_some(),
        loss_percent: params
            .proxy
            .map(|p| (p.loss_ratio * 100.0) as u8)
            .unwrap_or(0),
        control_records_expected: (params.clients * params.records_per_client) as u64,
        control_records_echoed: 0,
        bulk_parts_delivered: 0,
        datagrams_delivered: 0,
        dedup_dropped: 0,
        packets_forwarded: 0,
        packets_dropped: 0,
        transport_bytes_sent: 0,
        transport_bytes_recv: 0,
        note,
    }
}

fn write_summary(output: &Path, summary: &NetSummary<'_>) -> Result<(), XtaskError> {
    let path = output.join("summary.json");
    let body = serde_json::to_vec_pretty(summary).expect("summary is serializable");
    std::fs::write(&path, body).map_err(|source| XtaskError::Output {
        path: path.display().to_string(),
        source,
    })
}

fn write_metrics(
    output: &Path,
    report: &spall_net::TransportCheckReport,
) -> Result<(), XtaskError> {
    #[derive(Serialize)]
    struct ClientMetric {
        session: String,
        connected: bool,
        records_sent: u64,
        records_recv: u64,
        datagrams_sent: u64,
        datagrams_recv: u64,
        bulk_parts_recv: u64,
        dedup_dropped: u64,
    }
    #[derive(Serialize)]
    struct ProxyMetric {
        c2s_forwarded: u64,
        c2s_dropped: u64,
        s2c_forwarded: u64,
        s2c_dropped: u64,
    }
    #[derive(Serialize)]
    struct Metrics {
        clients_connected: usize,
        control_records_echoed: u64,
        datagrams_delivered: u64,
        bulk_parts_delivered: u64,
        dedup_dropped: u64,
        app_bytes_sent: u64,
        app_bytes_recv: u64,
        transport_bytes_sent: u64,
        transport_bytes_recv: u64,
        clients: Vec<ClientMetric>,
        proxies: Vec<ProxyMetric>,
    }

    let metrics = Metrics {
        clients_connected: report.clients_connected,
        control_records_echoed: report.control_records_echoed,
        datagrams_delivered: report.datagrams_delivered,
        bulk_parts_delivered: report.bulk_parts_delivered,
        dedup_dropped: report.dedup_dropped,
        app_bytes_sent: report.app_bytes_sent,
        app_bytes_recv: report.app_bytes_recv,
        transport_bytes_sent: report.transport_bytes_sent,
        transport_bytes_recv: report.transport_bytes_recv,
        clients: report
            .clients
            .iter()
            .map(|c| ClientMetric {
                session: c.session.clone(),
                connected: c.connected,
                records_sent: c.records_sent,
                records_recv: c.records_recv,
                datagrams_sent: c.datagrams_sent,
                datagrams_recv: c.datagrams_recv,
                bulk_parts_recv: c.bulk_parts_recv,
                dedup_dropped: c.dedup_dropped,
            })
            .collect(),
        proxies: report
            .proxy_stats
            .iter()
            .map(|s| ProxyMetric {
                c2s_forwarded: s.c2s_forwarded,
                c2s_dropped: s.c2s_dropped,
                s2c_forwarded: s.s2c_forwarded,
                s2c_dropped: s.s2c_dropped,
            })
            .collect(),
    };
    let path = output.join("metrics.json");
    let body = serde_json::to_vec_pretty(&metrics).expect("metrics serializable");
    std::fs::write(&path, body).map_err(|source| XtaskError::Output {
        path: path.display().to_string(),
        source,
    })
}
