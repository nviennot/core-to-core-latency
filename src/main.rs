use core_affinity::CoreId;
use std::sync::atomic::{AtomicU8, Ordering};
use std::io::Write;

#[cfg(feature = "rdtsc")]
use minstant::Instant;
#[cfg(not(feature = "rdtsc"))]
use std::time::Instant;
use clap::Parser;

const DEFAULT_NUM_ROUND_TRIP: RoundTrips = 30_000;
type RoundTrips = u32;

// This is a little matchup between two threads
// One sends a ball (Ping!), and the other sends it back (Pong!)
pub fn bench(
    (ping_core, pong_core): (CoreId, CoreId),
    num_round_trips: RoundTrips,
) -> std::time::Duration {
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

            for _ in 0..num_round_trips {
                while state.compare_exchange(Ping as u8, Pong as u8, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
            }
        });

        // Ping
        s.spawn(|| {
            core_affinity::set_for_current(ping_core);

            // Wait for Pong and start the timer
            while state.load(Ordering::Relaxed) == WaitForPong as u8 {}
            let start = Instant::now();

            for _ in 0..num_round_trips+1 {
                while state.compare_exchange(Pong as u8, Ping as u8, Ordering::Relaxed, Ordering::Relaxed).is_err() {}
            }

            start.elapsed()
        })
        .join()
        .unwrap()
    })
}

#[derive(clap::Parser)]
struct Args {
    /// The number of iterations
    #[clap(default_value_t = DEFAULT_NUM_ROUND_TRIP, value_parser)]
    num_round_trips: RoundTrips,

    /// Outputs CSV results on stdout
    #[clap(long, value_parser)]
    csv: bool
}

fn main() {
    let args = Args::parse();

    let cores = core_affinity::get_core_ids().expect("get_core_ids() failed");
    assert!(cores.len() >= 2);

    #[cfg(feature = "rdtsc")]
    let tsc = minstant::is_tsc_available();
    #[cfg(not(feature = "rdtsc"))]
    let tsc = false;

    eprintln!("Num cores: {}", cores.len());
    eprintln!("Num round trips: {}", args.num_round_trips);
    eprintln!("Using RDTSC to measure time: {}", tsc);
    eprintln!("Showing round-trip-time/2 in nanoseconds:");

    let mut results = Vec::new();

    // First print the column header
    eprint!("{: >5}", "");
    for j in &cores {
        eprint!("{: >5}", j.id);
        //        |||
        //        ||+-- Width
        //        |+--- Align
        //        +---- Fill
    }
    eprintln!();

    // Warmup
    bench((cores[0], cores[1]), 1000);

    for i in &cores {
        let mut row = Vec::new();
        eprint!("{: >5}", i.id);
        for j in &cores {
            let duration = if i.id != j.id {
                let duration = bench((*i, *j), args.num_round_trips);
                let duration = (duration.as_nanos() as f64) / (args.num_round_trips as f64) / 2.0;
                eprint!("{: >5.0}", duration);
                duration
            } else {
                eprint!("{: >5}", "");
                f64::NAN
            };
            let _ = std::io::stdout().lock().flush();
            row.push(duration);
        }
        results.push(row);
        eprintln!();
    }

    if args.csv {
        for row in &results {
            println!("{}", row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","));
        }
    }
}
