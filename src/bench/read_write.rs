use cache_padded::CachePadded;
use core_affinity::CoreId;
use std::sync::Barrier;
use std::sync::atomic::{Ordering, AtomicBool};
use quanta::Clock;

use super::Count;

pub struct Bench {
    barrier: CachePadded<Barrier>,
    owned_by_ping: CachePadded<AtomicBool>,
    owned_by_pong: CachePadded<AtomicBool>
}

impl Bench {
    pub fn new() -> Self {
        Self {
            barrier: CachePadded::new(Barrier::new(2)),
            owned_by_ping: Default::default(),
            owned_by_pong: Default::default(),
        }
    }
}

impl super::Bench for Bench {
    // Thread 1 writes to cache line 1 and read cache line 2
    // Thread 2 writes to cache line 2 and read cache line 1
    fn run(
        &self,
        (ping_core, pong_core): (CoreId, CoreId),
        clock: &Clock,
        num_round_trips: Count,
        num_samples: Count,
    ) -> Vec<f64> {
        let state = self;

        crossbeam_utils::thread::scope(|s| {
            let pong = s.spawn(move |_| {
                core_affinity::set_for_current(pong_core);
                state.barrier.wait();
                let mut v = false;
                for _ in 0..(num_round_trips*num_samples) {
                    // Acquire -> Release is important to enforce a causal dependency
                    // This has no effect on x86
                    while state.owned_by_ping.load(Ordering::Acquire) != v {}
                    state.owned_by_pong.store(!v, Ordering::Release);
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
                        while state.owned_by_pong.load(Ordering::Acquire) != v {}
                        state.owned_by_ping.store(v, Ordering::Release);
                        v = !v;
                    }
                    let end = clock.raw();
                    let duration = clock.delta(start, end).as_nanos();
                    results.push(duration as f64 / num_round_trips as f64 / 2.0);
                }
                results
            });

            pong.join().unwrap();
            ping.join().unwrap()
        }).unwrap()
    }
}
