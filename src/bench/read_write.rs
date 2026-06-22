use cache_padded::CachePadded;
use core_affinity::CoreId;
use std::sync::Barrier;
use std::sync::atomic::{Ordering, AtomicBool};
use quanta::Clock;

use super::Count;
use crate::numa::{MemPlacement, NumaBuffer};

const CACHELINE_SIZE: usize = 64;

/// One 64-byte cache line holding a single owner flag.
#[repr(C, align(64))]
struct OwnerLine {
    flag: AtomicBool,
    _padding: [u8; CACHELINE_SIZE - std::mem::size_of::<AtomicBool>()],
}

pub struct Bench {
    barrier: CachePadded<Barrier>,
    /// One buffer per NUMA placement; each holds the two owner cache lines
    /// (`[0]` = owned_by_ping, `[1]` = owned_by_pong).
    buffers: Vec<NumaBuffer>,
}

impl Bench {
    pub fn new(num_addresses: usize, placements: &[MemPlacement]) -> Result<Self, String> {
        if num_addresses != 1 {
            eprintln!("    WARN: num_addresses={} ignored for read_write (only supports 1)", num_addresses);
        }
        let buffers = placements
            .iter()
            .map(|&p| NumaBuffer::alloc(p, 2)
                .map_err(|e| format!("read_write: NUMA buffer allocation failed: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            barrier: CachePadded::new(Barrier::new(2)),
            buffers,
        })
    }

    fn lines(&self, domain_idx: usize) -> &[OwnerLine] {
        self.buffers[domain_idx].as_slice::<OwnerLine>(2)
    }
}

impl super::Bench for Bench {
    // Thread 1 writes to cache line 1 and read cache line 2
    // Thread 2 writes to cache line 2 and read cache line 1
    fn run(
        &self,
        domain_idx: usize,
        (ping_core, pong_core): (CoreId, CoreId),
        clock: &Clock,
        num_round_trips: Count,
        num_samples: Count,
    ) -> Vec<Vec<f64>> {
        let state = self;
        let lines = self.lines(domain_idx);
        let owned_by_ping = &lines[0].flag;
        let owned_by_pong = &lines[1].flag;

        // Reset owner flags for this run (all-zero == false initial state).
        owned_by_ping.store(false, Ordering::Relaxed);
        owned_by_pong.store(false, Ordering::Relaxed);

        crossbeam_utils::thread::scope(|s| {
            let pong = s.spawn(move |_| {
                core_affinity::set_for_current(pong_core);
                state.barrier.wait();
                let mut v = false;
                for _ in 0..(num_round_trips*num_samples) {
                    // Acquire -> Release is important to enforce a causal dependency
                    // This has no effect on x86
                    while owned_by_ping.load(Ordering::Acquire) != v {}
                    owned_by_pong.store(!v, Ordering::Release);
                    v = !v;
                }
            });

            let ping = s.spawn(move |_| {
                let mut results = Vec::with_capacity(num_samples as usize);

                core_affinity::set_for_current(ping_core);
                state.barrier.wait();
                let mut v = true;
                for _ in 0..num_samples {
                    let start = clock.raw();
                    for _ in 0..num_round_trips {
                        // Acquire -> Release is important to enforce a causal dependency
                        // This has no effect on x86
                        while owned_by_pong.load(Ordering::Acquire) != v {}
                        owned_by_ping.store(v, Ordering::Release);
                        v = !v;
                    }
                    let end = clock.raw();
                    let duration = clock.delta(start, end).as_nanos();
                    results.push(duration as f64 / num_round_trips as f64 / 2.0);
                }
                results
            });

            pong.join().unwrap();
            vec![ping.join().unwrap()]
        }).unwrap()
    }

    fn num_domains(&self) -> usize {
        self.buffers.len()
    }

    fn mem_numa_nodes(&self) -> Vec<i32> {
        self.buffers.iter().map(|b| b.node()).collect()
    }

    fn mem_numa_spanned(&self) -> Vec<bool> {
        self.buffers.iter().map(|b| b.spanned()).collect()
    }
}
