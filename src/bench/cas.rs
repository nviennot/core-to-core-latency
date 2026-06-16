use core_affinity::CoreId;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use quanta::Clock;
use super::Count;
use memmap2::MmapMut;
use crate::CliArgs;

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
    flags: Vec<CachelinePadded>,
}

impl Bench {
    pub fn new(num_addresses: usize) -> Self {
        let flags: Vec<CachelinePadded> = (0..num_addresses)
            .map(|_| CachelinePadded {
                flag: AtomicBool::new(PING),
                _padding: [0; CACHELINE_SIZE - std::mem::size_of::<AtomicBool>()],
            })
            .collect();
        Self {
            barrier: Barrier::new(2),
            flags,
        }
    }
}

impl super::Bench for Bench {

    fn run(
        &self,
        args: &CliArgs,
        (ping_core, pong_core): (CoreId, CoreId),
        clock: &Clock,
        num_round_trips: Count,
        num_samples: Count,
    ) -> Vec<Vec<f64>> {
        let state = self;
        let num_addresses = state.flags.len();

        let mut all_results = Vec::with_capacity(num_addresses);
        let mut mmap_flags = Vec::<CachelinePadded>::new();

        let flags = &{
            if args.no_recycling {
                core_affinity::set_for_current(ping_core);

                let size = CACHELINE_SIZE * num_addresses;

                let mmap = MmapMut::map_anon(size).expect("map_anon failed");

                mmap_flags = unsafe {
                    Vec::<CachelinePadded>::from_raw_parts(mmap.as_ptr() as *mut CachelinePadded, num_addresses, num_addresses)
                };
                let _ = std::mem::ManuallyDrop::new(mmap);
                for n in 0..mmap_flags.len() {
                    mmap_flags[n as usize].flag = AtomicBool::new(PING);
                }
                &mmap_flags
            } else {
                &state.flags
            }
        };

        for addr_idx in 0..num_addresses {
            let results = crossbeam_utils::thread::scope(|s| {
                let pong = s.spawn(move |_| {
                    core_affinity::set_for_current(pong_core);

                    state.barrier.wait();
                    let flag = &flags[addr_idx].flag;
                    for _ in 0..(num_round_trips*num_samples) {
                        while flag.compare_exchange(PING, PONG, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
                    }
                });

                let ping = s.spawn(move |_| {
                    core_affinity::set_for_current(ping_core);

                    let mut results = Vec::with_capacity(num_samples as usize);

                    state.barrier.wait();

                    let flag = &flags[addr_idx].flag;
                    for _ in 0..num_samples {
                        let start = clock.raw();
                        for _ in 0..num_round_trips {
                            while flag.compare_exchange(PONG, PING, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
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

            // Reset the flag for the next iteration
            state.flags[addr_idx].flag.store(PING, Ordering::Relaxed);
        }


        if args.no_recycling {
            let _ = std::mem::ManuallyDrop::new(mmap_flags);
        }

        all_results
    }

    fn num_addresses(&self) -> usize {
        self.flags.len()
    }

    fn address_ptrs(&self) -> Vec<usize> {
        self.flags.iter().map(|f| f as *const _ as usize).collect()
    }
}
