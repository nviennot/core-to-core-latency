use ansi_term::Color;
use core_affinity::CoreId;
use ndarray::{Axis, s};
use ordered_float::NotNan;
use std::sync::atomic::{AtomicU8, Ordering};
use std::io::Write;
use clap::Parser;

#[cfg(feature = "rdtsc")]
use minstant::Instant;
#[cfg(not(feature = "rdtsc"))]
use std::time::Instant;

const DEFAULT_NUM_SAMPLES: Count = 300;
const DEFAULT_NUM_ROUND_TRIPS: Count = 1000;
type Count = u32;

// This is a little matchup between two threads
// One sends a ball (Ping!), and the other sends it back (Pong!)
pub fn bench(
    (ping_core, pong_core): (CoreId, CoreId),
    num_round_trips: Count,
    num_samples: Count,
) -> Vec<std::time::Duration> {
    #[derive(PartialEq, Eq)]
    #[repr(u8)]
    enum State {
        WaitForPong,
        Pong,
        Ping,
    }
    use State::*;

    let state = AtomicU8::new(WaitForPong as u8);

    std::thread::scope(|s| {
        // Pong
        s.spawn(|| {
            core_affinity::set_for_current(pong_core);

            // Announce our presence
            state.store(Pong as u8, Ordering::Relaxed);

            for _ in 0..(num_round_trips*num_samples) {
                while state.compare_exchange(Ping as u8, Pong as u8, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
            }
        });

        // Ping
        s.spawn(|| {
            core_affinity::set_for_current(ping_core);

            let mut results = Vec::with_capacity(num_samples as usize);

            // Wait for Pong
            while state.load(Ordering::Relaxed) == WaitForPong as u8 {}

            for _ in 0..num_samples {
                let start = Instant::now();
                for _ in 0..num_round_trips {
                    while state.compare_exchange(Pong as u8, Ping as u8, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
                }
                results.push(start.elapsed());
            }

            results
        })
        .join()
        .unwrap()
    })
}

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

fn main() {
    let Args { num_samples, num_round_trips, csv } = Args::parse();

    let cores = core_affinity::get_core_ids().expect("get_core_ids() failed");
    assert!(cores.len() >= 2);
    let n_cores = cores.len();

    #[cfg(feature = "rdtsc")]
    let tsc = minstant::is_tsc_available();
    #[cfg(not(feature = "rdtsc"))]
    let tsc = false;

    eprintln!("Num cores: {}", n_cores);
    eprintln!("Using RDTSC to measure time: {}", tsc);
    eprintln!("Num round trips per samples: {}", num_round_trips);
    eprintln!("Num samples: {}", num_samples);

    let shape = ndarray::Ix3(n_cores, n_cores, num_samples as usize);
    let mut results = ndarray::Array::from_elem(shape, f64::NAN);

    // Warmup
    bench((cores[0], cores[1]), 300, 3);

    // First print the column header
    eprintln!("Showing latency=round-trip-time/2 in nanoseconds:");
    eprintln!();
    eprint!("{: >3}", "");
    for j in &cores {
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
        eprint!("{: >3}", core_i.id);
        for j in 0..n_cores {
            if i > j {
                let core_j = cores[j];
                // We add 1 warmup cycle first
                let durations = bench((core_i, core_j), num_round_trips, 1+num_samples);
                let durations = &durations[1..];
                let mut values = results.slice_mut(s![i,j,..]);
                for s in 0..num_samples as usize {
                    values[s] = durations[s].as_nanos() as f64 / (num_round_trips as f64) / 2.0;
                }

                let mean = format!("{: >4.0}", values.mean().unwrap());
                // We apply the central limit therem to estimate the standard deviation
                let stddev = format!("±{: <2.0}", values.std(1.0).min(99.0) / (num_samples as f64).sqrt());
                eprint!(" {}{}", mcolor.paint(mean), scolor.paint(stddev));
                let _ = std::io::stdout().lock().flush();
            }
        }
        eprintln!();
    }

    #[cfg(target_os = "macos")]
    eprintln!("{}", Color::Red.bold().paint("macOS may ignore thread-CPU affinity (we can't select a CPU to run on). Results may be innacuate"));

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

        eprintln!("Min  latency: {}ns {} cores: ({},{})", mcolor.paint(min_mean), scolor.paint(min_stddev), min_core_id_i, min_core_id_j);
        eprintln!("Max  latency: {}ns {} cores: ({},{})", mcolor.paint(max_mean), scolor.paint(max_stddev), max_core_id_i, max_core_id_j);
    }

    // Print mean latency
    {
        let values = results.iter().copied().filter(|v| !v.is_nan()).collect::<Vec<_>>();
        let values = ndarray::arr1(&values);
        let mean = format!("{:.1}", values.mean().unwrap());
        // no stddev, it's hard to put a value that is is meaningful without a lenthy explanation
        eprintln!("Mean latency: {}ns", mcolor.paint(mean));
    }

    if csv {
        let results = results.mean_axis(Axis(2)).unwrap();
        for row in results.rows() {
            let row = row.iter()
                .map(|v| v.is_nan().then_some("".to_string()).unwrap_or(v.to_string()))
                .collect::<Vec<_>>().join(",");
            println!("{}", row);
        }
    }
}
