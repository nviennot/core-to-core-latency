use core_affinity::CoreId;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use quanta::Clock;
use super::Count;
use crate::numa::{MemPlacement, NumaBuffer};

const PING: bool = false;
const PONG: bool = true;

const CACHELINE_SIZE: usize = 64;

#[repr(C, align(64))]
struct CachelinePadded {
    flag: AtomicBool,
    _padding: [u8; CACHELINE_SIZE - std::mem::size_of::<AtomicBool>()],
}

pub struct Bench {
    barrier: Barrier,
    num_addresses: usize,
    /// One shared-memory buffer per requested NUMA placement. Each buffer holds
    /// `num_addresses` cache lines, reinterpreted as `[CachelinePadded]`.
    buffers: Vec<NumaBuffer>,
}

impl Bench {
    pub fn new(num_addresses: usize, placements: &[MemPlacement]) -> Result<Self, String> {
        let buffers = placements
            .iter()
            .map(|&p| NumaBuffer::alloc(p, num_addresses)
                .map_err(|e| format!("cas: NUMA buffer allocation failed: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        // The all-zero pattern is exactly PING (false), so each buffer is
        // already in the correct initial state.
        Ok(Self {
            barrier: Barrier::new(2),
            num_addresses,
            buffers,
        })
    }

    fn flags(&self, domain_idx: usize) -> &[CachelinePadded] {
        self.buffers[domain_idx].as_slice::<CachelinePadded>(self.num_addresses)
    }
}

impl super::Bench for Bench {
    fn run(
        &self,
        domain_idx: usize,
        (ping_core, pong_core): (CoreId, CoreId),
        clock: &Clock,
        num_round_trips: Count,
        num_samples: Count,
    ) -> Vec<Vec<f64>> {
        let state = self;
        let flags = self.flags(domain_idx);
        let num_addresses = flags.len();

        let mut all_results = Vec::with_capacity(num_addresses);

        for addr_idx in 0..num_addresses {
            // Reset the flag for this run (all-zero == PING initial state).
            flags[addr_idx].flag.store(PING, Ordering::Relaxed);

            let results = crossbeam_utils::thread::scope(|s| {
                let pong = s.spawn(move |_| {
                    core_affinity::set_for_current(pong_core);

                    state.barrier.wait();
                    for _ in 0..(num_round_trips*num_samples) {
                        while flags[addr_idx].flag.compare_exchange(PING, PONG, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
                    }
                });

                let ping = s.spawn(move |_| {
                    core_affinity::set_for_current(ping_core);

                    let mut results = Vec::with_capacity(num_samples as usize);

                    state.barrier.wait();

                    for _ in 0..num_samples {
                        let start = clock.raw();
                        for _ in 0..num_round_trips {
                            while flags[addr_idx].flag.compare_exchange(PONG, PING, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
                        }
                        let end = clock.raw();
                        let duration = clock.delta(start, end).as_nanos();
                        results.push(duration as f64 / num_round_trips as f64 / 2.0);
                    }

                    results
                });

                pong.join().unwrap();
                ping.join().unwrap()
            }).unwrap();

            all_results.push(results);
        }

        all_results
    }

    fn num_addresses(&self) -> usize {
        self.num_addresses
    }

    fn num_domains(&self) -> usize {
        self.buffers.len()
    }

    fn address_ptrs(&self, domain_idx: usize) -> Vec<usize> {
        self.flags(domain_idx).iter().map(|f| f as *const _ as usize).collect()
    }

    fn mem_numa_nodes(&self) -> Vec<i32> {
        self.buffers.iter().map(|b| b.node()).collect()
    }

    fn mem_numa_spanned(&self) -> Vec<bool> {
        self.buffers.iter().map(|b| b.spanned()).collect()
    }
}
