use cache_padded::CachePadded;
use core_affinity::CoreId;
use std::sync::{Arc, Barrier};
use std::sync::atomic::{Ordering, AtomicU64};
use quanta::Clock;

use super::Count;
use crate::utils;

fn wait_for_non_zero_value(atomic_value: &AtomicU64, ordering: Ordering) -> u64 {
    loop {
        match atomic_value.load(ordering) {
            0 => continue,
            v => return v,
        }
    }
}

pub struct Bench;
impl super::Bench for Bench {
    // This test is not symmetric. We are doing one-way message passing.
    fn is_symmetric(&self) -> bool { false }

    // The two threads modify the same cacheline.
    // This is useful to benchmark spinlock performance.
    fn run(
        &self,
        (recv_core, send_core): (CoreId, CoreId),
        clock: Arc<Clock>,
        num_iterations: Count,
        num_samples: Count,
    ) -> Vec<f64> {
        struct State {
            barrier: Barrier,
            clocks: Vec<CachePadded<AtomicU64>>,
        }

        let clock_total_read_overhead = utils::clock_read_overhead(&clock, num_iterations);

        let clocks = (0..num_iterations).map(|_| Default::default()).collect();
        let state = Arc::new(State {
            barrier: Barrier::new(2),
            clocks,
        });

        // A shared time reference
        let start_time = clock.raw();

        let receiver = {
            let state = state.clone();
            let clock = clock.clone();
            std::thread::spawn(move || {
                core_affinity::set_for_current(recv_core);
                let mut results = Vec::with_capacity(num_samples as usize);

                state.barrier.wait();

                for _ in 0..num_samples as usize {
                    let mut latency: u64 = 0;

                    state.barrier.wait();
                    for v in &state.clocks {
                        // RDTSC is compensated below
                        let send_time = wait_for_non_zero_value(v, Ordering::Relaxed);
                        let recv_time = clock.raw().saturating_sub(start_time);
                        latency += recv_time.saturating_sub(send_time);
                    }
                    state.barrier.wait();

                    let total_latency = clock.delta(0, latency).saturating_sub(clock_total_read_overhead).as_nanos();
                    results.push(total_latency as f64 / num_iterations as f64);
                }

                results
            })
        };

        let sender = std::thread::spawn(move || {
            core_affinity::set_for_current(send_core);

            state.barrier.wait();

            for _ in 0..num_samples as usize {
                state.barrier.wait();
                for v in &state.clocks {
                    // Stall a bit to make sure the receiver is ready and we're not getting ahead of ourselves
                    // We could also put a state.barrier().wait(), but it's unclear whether it's a good
                    // idea due to additional generated traffic.
                    utils::delay_cycles(3000);

                    // max(1) to make sure the value is non-zero, which is what the receiver is waiting on
                    let send_time = clock.raw().saturating_sub(start_time).max(1);
                    v.store(send_time, Ordering::Relaxed);
                }

                state.barrier.wait();
                for v in &state.clocks {
                    v.store(0, Ordering::Relaxed);
                }
            }
        });

        sender.join().unwrap();
        receiver.join().unwrap()
    }
}
