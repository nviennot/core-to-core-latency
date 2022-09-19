use ansi_term::Color;
use cache_padded::CachePadded;
use core_affinity::CoreId;
use ndarray::{Axis, s};
use ordered_float::NotNan;
use std::sync::Arc;
use std::sync::atomic::{Ordering, AtomicBool};
use std::io::Write;
use std::time::Duration;
use clap::Parser;

#[cfg(feature = "rdtsc")]
use minstant::Instant;
#[cfg(not(feature = "rdtsc"))]
use std::time::Instant;

const DEFAULT_NUM_SAMPLES: Count = 300;
const DEFAULT_NUM_ROUND_TRIPS: Count = 1000;
type Count = u32;

const PING: bool = false;
const PONG: bool = true;

type BenchFn = fn((CoreId, CoreId), Count, Count) -> Vec<Duration>;

// The two threads modify the same cacheline.
// This is useful to benchmark spinlock performance.
// The first sample should be discarded
pub fn bench_ping_pong_on_single_cacheline(
    (ping_core, pong_core): (CoreId, CoreId),
    num_round_trips: Count,
    num_samples: Count,
) -> Vec<Duration> {
    // Not using std::thread::scope() because some users are using an older version of Rust
    // We'll go with an Arc.
    let state = Arc::new(AtomicBool::new(PING));

    let pong = {
        let state = state.clone();
        std::thread::spawn(move || {
            core_affinity::set_for_current(pong_core);

            for _ in 0..(num_round_trips*num_samples) {
                while state.compare_exchange(PING, PONG, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
            }
        })
    };

    let ping = std::thread::spawn(move || {
        core_affinity::set_for_current(ping_core);

        let mut results = Vec::with_capacity(num_samples as usize);

        for _ in 0..num_samples {
            let start = Instant::now();
            for _ in 0..num_round_trips {
                while state.compare_exchange(PONG, PING, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
            }
            results.push(start.elapsed());
        }

        results
    });

    pong.join().unwrap();
    ping.join().unwrap()
}


// Thread 1 writes to cache line 1 and read cache line 2
// Thread 2 writes to cache line 2 and read cache line 1
// This is useful to benchmark shared ring buffers, or other IPC mechanisms.
// The first sample should be discarded
pub fn bench_read_write_on_two_cachelines(
    (ping_core, pong_core): (CoreId, CoreId),
    num_round_trips: Count,
    num_samples: Count,
) -> Vec<Duration> {
    #[derive(Default)]
    struct State {
        owned_by_ping: CachePadded<AtomicBool>,
        owned_by_pong: CachePadded<AtomicBool>,
    }
    let state = Arc::new(State::default());

    let pong = {
        let state = state.clone();
        std::thread::spawn(move || {
            core_affinity::set_for_current(pong_core);
            let mut v = false;
            for _ in 0..(num_round_trips*num_samples) {
                // Acquire -> Release is important to enforce a causal dependency
                while state.owned_by_ping.load(Ordering::Acquire) == !v {}
                state.owned_by_pong.store(!v, Ordering::Release);
                v = !v;
            }
        })
    };

    let ping = std::thread::spawn(move || {
        let mut results = Vec::with_capacity(num_samples as usize);

        core_affinity::set_for_current(ping_core);
        let mut v = true;
        for _ in 0..num_samples {
            let start = Instant::now();
            for _ in 0..num_round_trips {
                // Acquire -> Release is important to enforce a causal dependency
                while state.owned_by_pong.load(Ordering::Acquire) == !v {}
                state.owned_by_ping.store(v, Ordering::Release);
                v = !v;
            }
            results.push(start.elapsed());
        }
        results
    });

    pong.join().unwrap();
    ping.join().unwrap()
}

#[derive(Clone)]
#[derive(clap::Parser)]
struct Args {
    /// The number of iterations per sample
    #[clap(default_value_t = DEFAULT_NUM_ROUND_TRIPS, value_parser)]
    num_round_trips: Count,

    /// The number of samples
    #[clap(default_value_t = DEFAULT_NUM_SAMPLES, value_parser)]
    num_samples: Count,

    /// Outputs the mean latencies in CSV format on stdout
    #[clap(long, value_parser)]
    csv: bool,
}

fn run_bench(cores: &[CoreId], args: Args, run_bench: BenchFn) {
    let Args { num_samples, num_round_trips, csv } = args;
    let n_cores = cores.len();
    assert!(n_cores >= 2);
    let shape = ndarray::Ix3(n_cores, n_cores, num_samples as usize);
    let mut results = ndarray::Array::from_elem(shape, f64::NAN);

    // Warmup
    run_bench((cores[0], cores[1]), 300, 3);

    // First print the column header
    eprint!("    {: >3}", "");
    for j in cores {
        eprint!(" {: >4}{: >3}", j.id, "");
        //        |||
        //        ||+-- Width
        //        |+--- Align
        //        +---- Fill
    }
    eprintln!();

    let mcolor = Color::White.bold();
    let scolor = Color::White.dimmed();

    // Do the benchmark
    for i in 0..n_cores {
        let core_i = cores[i];
        eprint!("    {: >3}", core_i.id);
        for j in 0..n_cores {
            if i > j {
                let core_j = cores[j];
                // We add 1 warmup cycle first
                let durations = run_bench((core_i, core_j), num_round_trips, 1+num_samples);
                let durations = &durations[1..];
                let mut values = results.slice_mut(s![i,j,..]);
                for s in 0..num_samples as usize {
                    values[s] = durations[s].as_nanos() as f64 / (num_round_trips as f64) / 2.0;
                }

                let mean = format!("{: >4.0}", values.mean().unwrap());
                // We apply the central limit theorem to estimate the standard deviation
                let stddev = format!("±{: <2.0}", values.std(1.0).min(99.0) / (num_samples as f64).sqrt());
                eprint!(" {}{}", mcolor.paint(mean), scolor.paint(stddev));
                let _ = std::io::stdout().lock().flush();
            }
        }
        eprintln!();
    }

    eprintln!();

    // Print min/max latency
    {
        let mean = results.mean_axis(Axis(2)).unwrap();
        let stddev = results.std_axis(Axis(2), 1.0) / (num_samples as f64).sqrt();

        let ((min_i, min_j), _) = mean.indexed_iter()
            .filter_map(|(i, v)| NotNan::new(*v).ok().map(|v| (i, v)))
            .min_by_key(|(_, v)| *v)
            .unwrap();
        let min_mean = format!("{:.1}", mean[(min_i, min_j)]);
        let min_stddev = format!("±{:.1}", stddev[(min_i, min_j)]);
        let (min_core_id_i, min_core_id_j) = (cores[min_i].id, cores[min_j].id);

        let ((max_i, max_j), _) = mean.indexed_iter()
            .filter_map(|(i, v)| NotNan::new(*v).ok().map(|v| (i, v)))
            .max_by_key(|(_, v)| *v)
            .unwrap();
        let max_mean = format!("{:.1}", mean[(max_i, max_j)]);
        let max_stddev = format!("±{:.1}", stddev[(max_i, max_j)]);
        let (max_core_id_i, max_core_id_j) = (cores[max_i].id, cores[max_j].id);

        eprintln!("    Min  latency: {}ns {} cores: ({},{})", mcolor.paint(min_mean), scolor.paint(min_stddev), min_core_id_i, min_core_id_j);
        eprintln!("    Max  latency: {}ns {} cores: ({},{})", mcolor.paint(max_mean), scolor.paint(max_stddev), max_core_id_i, max_core_id_j);
    }

    // Print mean latency
    {
        let values = results.iter().copied().filter(|v| !v.is_nan()).collect::<Vec<_>>();
        let values = ndarray::arr1(&values);
        let mean = format!("{:.1}", values.mean().unwrap());
        // no stddev, it's hard to put a value that is meaningful without a lengthy explanation
        eprintln!("    Mean latency: {}ns", mcolor.paint(mean));
    }

    if csv {
        let results = results.mean_axis(Axis(2)).unwrap();
        for row in results.rows() {
            let row = row.iter()
                .map(|v| if v.is_nan() { "".to_string() } else { v.to_string() })
                .collect::<Vec<_>>().join(",");
            println!("{}", row);
        }
    }
}

fn main() {
    let args = Args::parse();

    let cores = core_affinity::get_core_ids().expect("get_core_ids() failed");

    #[cfg(feature = "rdtsc")]
    let tsc = minstant::is_tsc_available();
    #[cfg(not(feature = "rdtsc"))]
    let tsc = false;

    eprintln!("Num cores: {}", cores.len());
    eprintln!("Using RDTSC to measure time: {}", tsc);
    eprintln!("Num round trips per samples: {}", args.num_round_trips);
    eprintln!("Num samples: {}", args.num_samples);
    #[cfg(target_os = "macos")]
    eprintln!("{}", Color::Red.bold().paint("macOS may ignore thread-CPU affinity (we can't select a CPU to run on). Results may be inaccurate"));

    eprintln!("");
    eprintln!("1) Ping/Pong latency on a single shared cache line (used in spinlocks)");
    eprintln!("");
    run_bench(&cores, args.clone(), bench_ping_pong_on_single_cacheline);

    eprintln!("");
    eprintln!("2) Causal write then read latency on two cache lines, (used in ringbuffers)");
    eprintln!("");
    run_bench(&cores, args.clone(), bench_read_write_on_two_cachelines);
}
