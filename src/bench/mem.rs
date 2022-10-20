use cache_padded::CachePadded;
use core_affinity::CoreId;
use std::sync::atomic::{AtomicBool, Ordering};
use std::cell::UnsafeCell;
use quanta::Clock;
use super::Count;

const KB: usize = 1024;
const GB: usize = 1024*1024*1024;
const MEM_SIZE: usize = 256*KB;

const DATA_PRODUCED: bool = false;
const DATA_CONSUMED: bool = true;

type Value = u128;
const N_CHUNKS: usize = 4;
const N_VALUES_PER_CHUNKS: usize = MEM_SIZE / N_CHUNKS / std::mem::size_of::<Value>();

#[repr(align(4096))]
struct MemChunk(UnsafeCell<[Value; N_VALUES_PER_CHUNKS]>);

impl MemChunk {
    fn new() -> Self {
        Self([0; N_VALUES_PER_CHUNKS].into())
    }
}

unsafe impl Sync for MemChunk {}

pub struct Bench {
    data: Box<[MemChunk; N_CHUNKS]>,
    flag: [CachePadded<AtomicBool>; N_CHUNKS],
}

impl Bench {
    pub fn new() -> Self {
        Self {
            data: Box::new(std::array::from_fn(|_| MemChunk::new())),
            flag: std::array::from_fn(|_| AtomicBool::new(DATA_CONSUMED).into()),
        }
    }
}

impl super::Bench for Bench {
    fn is_symmetric(&self) -> bool { false }

    // The two threads modify the same cacheline.
    // This is useful to benchmark spinlock performance.
    fn run(
        &self,
        (ping_core, pong_core): (CoreId, CoreId),
        clock: &Clock,
        num_round_trips: Count,
        num_samples: Count,
    ) -> Vec<f64> {
        crossbeam_utils::thread::scope(|s| {
            let send_thread = s.spawn(move |_| {
                core_affinity::set_for_current(pong_core);

                for r in 0..(num_round_trips*num_samples) {
                    for i in 0..N_CHUNKS {
                        let data = unsafe { &mut *self.data[i].0.get() };

                        while self.flag[i].load(Ordering::Relaxed) != DATA_CONSUMED {}
                        unsafe { std::ptr::write_bytes(data, r as u8, 1); } // memset
                        self.flag[i].store(DATA_PRODUCED, Ordering::Release);
                    }
                }
            });

            let recv_thread = s.spawn(move |_| {
                core_affinity::set_for_current(ping_core);

                let mut results = Vec::with_capacity(num_samples as usize);

                for _ in 0..num_samples {
                    let start = clock.raw();

                    for _ in 0..num_round_trips {
                        for i in 0..N_CHUNKS {
                            let data = unsafe { &mut *self.data[i].0.get() };

                            while self.flag[i].load(Ordering::Acquire) != DATA_PRODUCED {}
                            // there's no fast reads without copies.
                            for d in data { unsafe { (d as *const Value).read_volatile() }; }
                            self.flag[i].store(DATA_CONSUMED, Ordering::Relaxed);
                        }
                    }

                    let end = clock.raw();
                    let duration = clock.delta(start, end).as_nanos() as f64;

                    let throughput_in_gb_per_sec = (MEM_SIZE as f64 * num_round_trips as f64)/(duration * ((GB as f64) * 1e-9));
                    results.push(throughput_in_gb_per_sec);
                }

                results
            });

            send_thread.join().unwrap();
            recv_thread.join().unwrap()
        }).unwrap()
    }
}
