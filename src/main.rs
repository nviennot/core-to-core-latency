use core_affinity::CoreId;
use std::sync::atomic::{AtomicU8, Ordering};
use std::io::Write;

#[cfg(feature = "rdtsc")]
use minstant::Instant;
#[cfg(not(feature = "rdtsc"))]
use std::time::Instant;

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

fn main() {
    // "./prog [num_round_trips]"
    let args: Vec<String> = std::env::args().collect();
    let num_round_trips: RoundTrips = args.get(1).map(|n| n.parse().unwrap())
        .unwrap_or(DEFAULT_NUM_ROUND_TRIP);

    let cores = core_affinity::get_core_ids().expect("get_core_ids() failed");
    assert!(cores.len() >= 2);

    #[cfg(feature = "rdtsc")]
    let tsc = minstant::is_tsc_available();
    #[cfg(not(feature = "rdtsc"))]
    let tsc = false;

    println!("Num cores: {}", cores.len());
    println!("Num round trips: {}", num_round_trips);
    println!("Using RDTSC to measure time: {}", tsc);
    println!("Showing round-trip-time/2 in nanoseconds:");

    // First print the column header
    print!("{: >5}", "");
    for j in &cores {
        print!("{: >5}", j.id);
        //        |||
        //        ||+-- Width
        //        |+--- Align
        //        +---- Fill
    }
    println!();

    // Warmup
    bench((cores[0], cores[1]), 1000);

    for i in &cores {
        print!("{: >5}", i.id);
        for j in &cores {
            if i.id != j.id {
                let duration = bench((*i, *j), num_round_trips) / num_round_trips / 2;
                print!("{: >5}", duration.as_nanos());
            } else {
                print!("{: >5}", "");
            }
            let _ = std::io::stdout().lock().flush();
        }
        println!();
    }
}
