//! Baseline size / peak-allocation measurement over a retained save (ENG-30 / T23 reconnect).
//!
//! Ignored: point `SPALL_RETAINED_SAVE` at a `world.db` (a *copy* -- recovery opens it read-write)
//! and run
//! `cargo test -p spall_server --release --test baseline_measure -- --ignored --nocapture`.
//! Reports, for terrain vs body geometry vs metadata: bricks, dense/uniform split, raw postcard
//! bytes, and the peak heap allocation of each stage of building a baseline transfer.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use spall_physics::PhysicsConfig;
use spall_protocol::baseline::{BaselineCells, BaselineOwner, BaselineWorld};
use spall_server::persist::{self, PersistConfig};
use spall_server::{baseline, logical_world_baseline};
use spall_store::Writer;
use spall_structure::AnchorPlane;

struct Counting;
static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = CUR.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CUR.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let now = CUR.fetch_add(new - l.size(), Ordering::Relaxed) + (new - l.size());
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CUR.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Runs `f`, returning its result and the peak *additional* heap bytes it held live.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CUR.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

fn mib(b: usize) -> String {
    format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0))
}

#[derive(Default)]
struct Class {
    volumes: usize,
    bricks: usize,
    dense: usize,
    uniform: usize,
    raw_cells_bytes: usize,
    raw_total_bytes: usize,
    largest_volume_bricks: usize,
}

#[test]
#[ignore = "needs SPALL_RETAINED_SAVE (a copy of a retained world.db)"]
fn retained_save_baseline_breakdown() {
    let path = PathBuf::from(
        std::env::var("SPALL_RETAINED_SAVE").expect("SPALL_RETAINED_SAVE=path/to/world.db"),
    );
    let writer = Writer::open(&path).expect("open save");
    let recovery = writer.recover().expect("recover");
    let cfg = PersistConfig {
        world_id: spall_server::serve::T10_WORLD_ID,
        seed: 0,
        generator_version: 1,
    };
    let (sim, _seq) = {
        let (r, peak) = peak_of(|| {
            persist::restore(
                &recovery,
                &cfg,
                persist::RecoveryChoice::RequireClean,
                spall_sim::fixtures::stone_manifest(),
                AnchorPlane::at(0),
                PhysicsConfig::default(),
            )
            .expect("restore")
        });
        println!("restore peak additional heap: {}", mib(peak));
        r
    };

    let (world, peak_snapshot) = peak_of(|| logical_world_baseline(&sim, None));
    println!(
        "logical_world_baseline: peak additional {} (resident world copy {})",
        mib(peak_snapshot),
        mib(world
            .volumes
            .iter()
            .flat_map(|v| &v.bricks)
            .map(|b| match &b.cells {
                BaselineCells::Dense(d) => d.len() * 2,
                BaselineCells::Uniform(_) => 2,
            })
            .sum())
    );

    let mut terrain = Class::default();
    let mut bodies = Class::default();
    for v in &world.volumes {
        let raw = postcard::to_stdvec(v).unwrap().len();
        let cells: usize = v
            .bricks
            .iter()
            .map(|b| postcard::to_stdvec(&b.cells).unwrap().len())
            .sum();
        let c = match v.owner {
            BaselineOwner::Terrain => &mut terrain,
            BaselineOwner::Body(_) => &mut bodies,
        };
        c.volumes += 1;
        c.bricks += v.bricks.len();
        c.raw_total_bytes += raw;
        c.raw_cells_bytes += cells;
        c.largest_volume_bricks = c.largest_volume_bricks.max(v.bricks.len());
        for b in &v.bricks {
            match b.cells {
                BaselineCells::Dense(_) => c.dense += 1,
                BaselineCells::Uniform(_) => c.uniform += 1,
            }
        }
    }
    for (name, c) in [("terrain", &terrain), ("bodies", &bodies)] {
        println!(
            "{name}: {} volumes, {} bricks ({} dense, {} uniform), largest volume {} bricks; \
             raw postcard {} of which cell payload {} and metadata (ids, coords, revisions, flags) {}",
            c.volumes,
            c.bricks,
            c.dense,
            c.uniform,
            c.largest_volume_bricks,
            mib(c.raw_total_bytes),
            mib(c.raw_cells_bytes),
            mib(c.raw_total_bytes - c.raw_cells_bytes),
        );
    }

    // Sparsity of dense bricks: how many cells are non-air, and how many distinct values a
    // brick holds (informs a palette / run-length brick encoding, a separate option from
    // segmenting the transfer).
    let (mut nonzero, mut total, mut sparse_lt_1pct, mut single_value) =
        (0u64, 0u64, 0usize, 0usize);
    for v in &world.volumes {
        for b in &v.bricks {
            if let BaselineCells::Dense(d) = &b.cells {
                let nz = d.iter().filter(|c| **c != 0).count() as u64;
                nonzero += nz;
                total += d.len() as u64;
                if nz * 100 < d.len() as u64 {
                    sparse_lt_1pct += 1;
                }
                let mut vals: Vec<u16> = d.clone();
                vals.sort_unstable();
                vals.dedup();
                if vals.len() <= 2 {
                    single_value += 1;
                }
            }
        }
    }
    println!(
        "dense bricks: {nonzero} of {total} cells non-air ({:.2}%); {sparse_lt_1pct} bricks under 1% occupied; {single_value} bricks hold at most 2 distinct values",
        100.0 * nonzero as f64 / total.max(1) as f64
    );

    let (raw, peak_encode) = peak_of(|| world.encode());
    println!(
        "encode (single blob): {} raw, peak additional {}",
        mib(raw.len()),
        mib(peak_encode)
    );
    println!(
        "cap MAX_ASSEMBLED_TRANSFER_DECOMPRESSED = {}",
        mib(spall_protocol::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED)
    );
    let (compressed, peak_compress) = peak_of(|| world.encode_compressed());
    println!(
        "encode_compressed: {} on the wire, peak additional {} (excludes the {} raw input)",
        mib(compressed.len()),
        mib(peak_compress),
        mib(raw.len())
    );
    // Client side of the same world (the compressed form is only 0.2 MiB, so the client could
    // receive it; what would it hold?). Mirrors `receive_baseline_body` + `install_baseline_world`.
    drop(raw);
    let (decoded, peak_decode) = peak_of(|| BaselineWorld::decode_compressed(&compressed));
    let decoded = match decoded {
        Ok(d) => d,
        Err(e) => {
            // The receiver enforces the same 256 MiB decompressed bound, so even a 0.2 MiB
            // wire payload is refused client-side: the failure is symmetric.
            println!(
                "client decode_compressed REFUSES the transfer: {e:?} (peak additional {})",
                mib(peak_decode)
            );
            return;
        }
    };
    println!(
        "client decode_compressed: peak additional {}",
        mib(peak_decode)
    );
    let (_, peak_hash) = peak_of(|| spall_protocol::Hash32::of(&decoded.encode()));
    println!(
        "client canonical re-encode for end.assembled_hash: peak additional {}",
        mib(peak_hash)
    );
    let (replica, peak_install) = peak_of(|| {
        let mut r = spall_client::ReplicaWorld::empty(spall_client::ReplicaConfig::default());
        r.install_baseline_world(&decoded).expect("install");
        r
    });
    println!(
        "client install_baseline_world: peak additional {} (retained after install)",
        mib(peak_install)
    );
    drop(replica);
    let _ = baseline::chunk_payload;
    let _: Option<BaselineWorld> = None;
}

/// The regression case for the segmented baseline (see `docs/reports/large-world-baseline-design.md`):
/// the retained save must yield a transfer without raising the cap. Fails today with
/// `BaselineError::TooLarge` (377 MiB raw > 256 MiB); enable once segmented capture lands.
#[test]
#[ignore = "known failing until segmented baselines land; needs SPALL_RETAINED_SAVE"]
fn retained_save_yields_a_bounded_baseline_transfer() {
    let path = PathBuf::from(
        std::env::var("SPALL_RETAINED_SAVE").expect("SPALL_RETAINED_SAVE=path/to/world.db"),
    );
    let writer = Writer::open(&path).expect("open save");
    let recovery = writer.recover().expect("recover");
    let cfg = PersistConfig {
        world_id: spall_server::serve::T10_WORLD_ID,
        seed: 0,
        generator_version: 1,
    };
    let (sim, _) = persist::restore(
        &recovery,
        &cfg,
        persist::RecoveryChoice::RequireClean,
        spall_sim::fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .expect("restore");
    let (result, peak) = peak_of(|| {
        spall_server::logical_capture_transfer(
            &sim,
            None,
            spall_protocol::TransferId(1),
            spall_protocol::InterestEpoch(1),
            spall_core::JournalSeq(0),
        )
    });
    println!("capture peak additional heap: {}", mib(peak));
    assert!(result.is_ok(), "{:?}", result.err());
}

/// Peak additional heap and retained heap of a segmented transfer of the retained save: server
/// capture (snapshot, encode), client receive (temporary vs staged world), install, and the memory
/// held with 1, 4 and 8 concurrent joiners plus a slow recipient. Measurements only report; the
/// acceptance asserts are the last few lines.
#[test]
#[ignore = "needs SPALL_RETAINED_SAVE (a copy of a retained world.db)"]
fn retained_save_segmented_transfer_memory() {
    use spall_client::segmented::SegmentedReceiver;
    use spall_client::{ReplicaConfig, ReplicaWorld};
    use spall_protocol::{InterestEpoch, TransferId};
    use spall_server::baseline::{snapshot_world, transfer_from_snapshot_segmented};

    let path = PathBuf::from(
        std::env::var("SPALL_RETAINED_SAVE").expect("SPALL_RETAINED_SAVE=path/to/world.db"),
    );
    let writer = Writer::open(&path).expect("open save");
    let recovery = writer.recover().expect("recover");
    let cfg = PersistConfig {
        world_id: spall_server::serve::T10_WORLD_ID,
        seed: 0,
        generator_version: 1,
    };
    let (sim, _) = persist::restore(
        &recovery,
        &cfg,
        persist::RecoveryChoice::RequireClean,
        spall_sim::fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .expect("restore");
    drop(recovery);
    let authoritative_hash = sim.world().world_hash();
    println!(
        "world: {} bodies, hash {}",
        sim.world().body_count(),
        authoritative_hash
    );

    let caps: Vec<usize> = std::env::var("SEGMENT_CAPS_MIB")
        .unwrap_or_else(|_| "1,4,16".to_string())
        .split(',')
        .map(|c| c.trim().parse::<usize>().unwrap() << 20)
        .collect();
    for cap in caps {
        println!("=== segment budget {} ===", mib(cap));
        // Server: the snapshot (COW handles), then the streamed encode.
        let (snapshot, peak_snapshot) = peak_of(|| snapshot_world(&sim, None));
        println!(
            "server snapshot_world: peak additional {}",
            mib(peak_snapshot)
        );
        let est = snapshot.estimated_decoded_bytes();
        let heap_before = CUR.load(Ordering::Relaxed);
        let started = std::time::Instant::now();
        let (transfer, peak_encode) = peak_of(|| {
            transfer_from_snapshot_segmented(snapshot, TransferId(1), InterestEpoch(1), cap)
                .expect("segmented capture")
        });
        let encode_ms = started.elapsed().as_millis();
        let retained = CUR.load(Ordering::Relaxed).saturating_sub(heap_before);
        let stats = transfer.segments.unwrap();
        println!(
            "server encode: peak additional {} (estimate of decoded world {}), {} ms; retained after {} (parts, {} on the wire); {} segments, max segment decoded {}",
            mib(peak_encode),
            mib(est as usize),
            encode_ms,
            mib(retained),
            mib(transfer.payload_bytes()),
            stats.segments,
            mib(stats.max_segment_decoded as usize)
        );

        // Joiners share the compressed parts: heap held with 1, 4 and 8 reissued transfers.
        for n in [1u64, 4, 8] {
            let base = CUR.load(Ordering::Relaxed);
            let joiners: Vec<_> = (0..n)
                .map(|i| transfer.reissue(TransferId(10 + i)))
                .collect();
            let extra = CUR.load(Ordering::Relaxed).saturating_sub(base);
            println!(
                "  {n} concurrent joiners share the parts: extra retained heap {} bytes",
                extra
            );
            // A slow recipient holds one transient stamped part at a time per joiner.
            let (_, peak_slow) = peak_of(|| {
                let mut held = Vec::new();
                for j in &joiners {
                    let mut part = j.parts[0].clone();
                    part.transfer_id = j.begin.transfer_id;
                    held.push(part);
                }
                held.len()
            });
            println!(
                "  {n} slow recipients each mid-part: peak additional {} bytes",
                peak_slow
            );
            drop(joiners);
        }

        // Client: feed the payload the way the bulk stream delivers it, one part at a time.
        let parts = std::sync::Arc::clone(&transfer.parts);
        let heap_before = CUR.load(Ordering::Relaxed);
        let started = std::time::Instant::now();
        let (receipt, peak_recv) = peak_of(|| {
            let mut rx = SegmentedReceiver::new(transfer.begin.checkpoint_tick.get(), None);
            for p in parts.iter() {
                rx.push(&p.payload).expect("segment validates");
            }
            rx.finish().expect("transfer complete")
        });
        let recv_ms = started.elapsed().as_millis();
        let staged_bytes = CUR.load(Ordering::Relaxed).saturating_sub(heap_before);
        assert_eq!(receipt.chain_hash, transfer.end.assembled_hash);
        println!(
            "client receive: peak additional {} = staged world {} (retained) + temporary ~{}; {} ms; max segment decoded {}, max frame buffered {}",
            mib(peak_recv),
            mib(staged_bytes),
            mib(peak_recv.saturating_sub(staged_bytes)),
            recv_ms,
            mib(receipt.max_segment_decoded as usize),
            mib(receipt.max_buffered_bytes)
        );
        let mut replica = ReplicaWorld::empty(ReplicaConfig::default());
        let (_, peak_install) =
            peak_of(|| replica.install_staged(receipt.staged).expect("install"));
        println!(
            "client install_staged (swap): peak additional {}",
            mib(peak_install)
        );
        assert_eq!(
            replica.world_hash(),
            authoritative_hash,
            "exact authoritative topology"
        );
        assert_eq!(replica.body_ids().count(), sim.world().body_count());
        drop(replica);
        drop(transfer);
        drop(parts);
    }
}
