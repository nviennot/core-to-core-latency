use core_affinity::CoreId;
use std::sync::{Arc, Barrier};
use std::sync::atomic::{Ordering, AtomicBool};
use quanta::{Clock};
use super::Count;

const PING: bool = false;
const PONG: bool = true;

pub struct Bench;
impl super::Bench for Bench {
    // The two threads modify the same cacheline.
    // This is useful to benchmark spinlock performance.
    fn run(
        &self,
        (ping_core, pong_core): (CoreId, CoreId),
        clock: Arc<Clock>,
        num_round_trips: Count,
        num_samples: Count,
    ) -> Vec<f64> {
        struct State {
            barrier: Barrier,
            flag: AtomicBool,
        }

        // Not using std::thread::scope() because some users are using an older version of Rust
        // We'll go with an Arc.
        let state = Arc::new(State {
            barrier: Barrier::new(2),
            flag: AtomicBool::new(PING),
        });

        let pong = {
            let state = state.clone();
            std::thread::spawn(move || {
                core_affinity::set_for_current(pong_core);

                state.barrier.wait();
                for _ in 0..(num_round_trips*num_samples) {
                    while state.flag.compare_exchange(PING, PONG, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
                }
            })
        };

        let ping = std::thread::spawn(move || {
            core_affinity::set_for_current(ping_core);

            let mut results = Vec::with_capacity(num_samples as usize);

            state.barrier.wait();

            for _ in 0..num_samples {
                let start = clock.raw();
                for _ in 0..num_round_trips {
                    while state.flag.compare_exchange(PONG, PING, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
                }
                let end = clock.raw();
                let duration = clock.delta(start, end).as_nanos();
                results.push(duration as f64 / num_round_trips as f64 / 2.0);
            }

            results
        });

        pong.join().unwrap();
        ping.join().unwrap()
    }
}
