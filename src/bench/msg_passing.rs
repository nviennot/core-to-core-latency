use core_affinity::CoreId;
use std::sync::Barrier;
use std::sync::atomic::{Ordering, AtomicU64};
use quanta::Clock;

use super::Count;
use crate::utils;
use crate::numa::{MemPlacement, NumaBuffer};

const CACHELINE_SIZE: usize = 64;

/// One 64-byte cache line holding a single clock slot.
#[repr(C, align(64))]
struct ClockLine {
    value: AtomicU64,
    _padding: [u8; CACHELINE_SIZE - std::mem::size_of::<AtomicU64>()],
}

pub struct Bench {
    barrier: Barrier,
    num_iterations: usize,
    /// One buffer per NUMA placement; each holds `num_iterations` clock cache
    /// lines, reinterpreted as `[ClockLine]`.
    buffers: Vec<NumaBuffer>,
}

impl Bench {
    pub fn new(num_iterations: u32, num_addresses: usize, placements: &[MemPlacement]) -> Result<Self, String> {
        if num_addresses != 1 {
            eprintln!("    WARN: num_addresses={} ignored for msg_passing (only supports 1)", num_addresses);
        }
        let num_iterations = num_iterations as usize;
        let buffers = placements
            .iter()
            .map(|&p| NumaBuffer::alloc(p, num_iterations)
                .map_err(|e| format!("msg_passing: NUMA buffer allocation failed: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            barrier: Barrier::new(2),
            num_iterations,
            buffers,
        })
    }

    fn clocks(&self, domain_idx: usize) -> &[ClockLine] {
        self.buffers[domain_idx].as_slice::<ClockLine>(self.num_iterations)
    }
}

impl super::Bench for Bench {
    // This test is not symmetric. We are doing one-way message passing.
    fn is_symmetric(&self) -> bool { false }

    fn run(
        &self,
        domain_idx: usize,
        (recv_core, send_core): (CoreId, CoreId),
        clock: &Clock,
        num_iterations: Count,
        num_samples: Count,
    ) -> Vec<Vec<f64>> {
        let clock_read_overhead_sum = utils::clock_read_overhead_sum(clock, num_iterations);

        // A shared time reference
        let start_time = clock.raw();
        let state = self;
        let clocks = self.clocks(domain_idx);

        // Reset clock slots for this run (all-zero == 0 initial state).
        for v in clocks {
            v.value.store(0, Ordering::Relaxed);
        }

        crossbeam_utils::thread::scope(|s| {
            let receiver = s.spawn(|_| {
                core_affinity::set_for_current(recv_core);
                let mut results = Vec::with_capacity(num_samples as usize);

                state.barrier.wait();

                for _ in 0..num_samples as usize {
                    let mut latency: u64 = 0;

                    state.barrier.wait();
                    for v in clocks {
                        // RDTSC is compensated below
                        let send_time = wait_for_non_zero_value(&v.value, Ordering::Relaxed);
                        let recv_time = clock.raw().saturating_sub(start_time);
                        latency += recv_time.saturating_sub(send_time);
                    }
                    state.barrier.wait();

                    let total_latency = clock.delta(0, latency).saturating_sub(clock_read_overhead_sum).as_nanos();
                    results.push(total_latency as f64 / num_iterations as f64);
                }

                results
            });

            let sender = s.spawn(|_| {
                core_affinity::set_for_current(send_core);

                state.barrier.wait();

                for _ in 0..num_samples as usize {
                    state.barrier.wait();
                    for v in clocks {
                        // Stall a bit to make sure the receiver is ready and we're not getting ahead of ourselves
                        // We could also put a state.barrier().wait(), but it's unclear whether it's a good
                        // idea due to additional generated traffic.
                        utils::delay_cycles(10000);

                        // max(1) to make sure the value is non-zero, which is what the receiver is waiting on
                        let send_time = clock.raw().saturating_sub(start_time).max(1);
                        v.value.store(send_time, Ordering::Relaxed);
                    }

                    state.barrier.wait();
                    for v in clocks {
                        v.value.store(0, Ordering::Relaxed);
                    }
                }
            });

            sender.join().unwrap();
            vec![receiver.join().unwrap()]
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

fn wait_for_non_zero_value(atomic_value: &AtomicU64, ordering: Ordering) -> u64 {
    loop {
        match atomic_value.load(ordering) {
            0 => continue,
            v => return v,
        }
    }
}
