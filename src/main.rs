mod bench;
mod utils;

use bench::Count;
use std::collections::HashSet;
use std::sync::Arc;
use clap::Parser;
use core_affinity::CoreId;
use quanta::Clock;
use crate::bench::run_bench;

const DEFAULT_NUM_SAMPLES: Count = 300;
const DEFAULT_NUM_ITERATIONS_PER_SAMPLE: Count = 1000;
const DEFAULT_NUM_ADDRESSES: usize = 1;

#[derive(Clone)]
#[derive(clap::Parser)]
pub struct CliArgs {
    /// The number of iterations per sample
    #[clap(default_value_t = DEFAULT_NUM_ITERATIONS_PER_SAMPLE, value_parser)]
    num_iterations: Count,

    /// The number of samples
    #[clap(default_value_t = DEFAULT_NUM_SAMPLES, value_parser)]
    num_samples: Count,

    /// The number of addresses to test (each address is a separate cacheline)
    #[clap(long, default_value_t = DEFAULT_NUM_ADDRESSES, value_parser)]
    pub num_addresses: usize,

    /// Output CSV files with latency data. When enabled, writes two files per benchmark: {n}
    ///   <prefix>.<bench>.per_address.csv - Per-address latency for each core pair {n}
    ///   <prefix>.<bench>.summary.csv - Summary with mean-of-means and mean-of-medians {n}
    /// Note: Mean-based statistics can be skewed by occasional high-latency samples {n}
    /// caused by interrupts or scheduling. Median-based statistics are more robust.
    #[clap(long, value_parser)]
    csv: bool,

    /// Prefix for CSV output files (default: "report")
    #[clap(long, default_value = "report", value_parser)]
    pub csv_output_prefix: String,

    /// Select which benchmark to run, in a comma delimited list, e.g., '1,3' {n}
    /// 1: CAS latency on a single shared cache line. {n}
    /// 2: Single-writer single-reader latency on two shared cache lines. {n}
    /// 3: One writer and one reader on many cache line, using the clock.
    #[clap(short, long, default_value="1", require_delimiter=true, value_delimiter=',', value_parser)]
    bench: Vec<usize>,

    /// Specify the cores by id. Supports individual IDs and inclusive ranges. {n}
    /// Examples: --cores 7,11,13  or  --cores 9-44,56-63  or  --cores 0-19 {n}
    /// By default all cores are used.
    #[clap(short, long, require_delimiter=true, value_delimiter=',', value_parser)]
    cores: Vec<String>,

    /// Enable allocating fresh anonymous pages for each core pairs. {n}
    /// Those are "leaked" to insure they do not get reused until the benchmark ends.
    #[clap(long, value_parser)]
    no_recycling: bool,
}

fn parse_cores(specs: &[String], all_cores: &[CoreId]) -> Vec<CoreId> {
    if specs.is_empty() {
        return all_cores.to_vec();
    }

    let valid_ids: HashSet<usize> = all_cores.iter().map(|c| c.id).collect();
    let mut selected_ids: Vec<usize> = Vec::new();

    for spec in specs {
        let spec = spec.trim();
        if spec.contains('-') {
            let parts: Vec<&str> = spec.splitn(2, '-').collect();
            let start: usize = parts[0].parse()
                .unwrap_or_else(|_| panic!("Invalid range start in '{}': '{}'", spec, parts[0]));
            let end: usize = parts[1].parse()
                .unwrap_or_else(|_| panic!("Invalid range end in '{}': '{}'", spec, parts[1]));
            if start > end {
                panic!("Invalid range '{}': start ({}) > end ({})", spec, start, end);
            }
            for id in start..=end {
                selected_ids.push(id);
            }
        } else {
            let id: usize = spec.parse()
                .unwrap_or_else(|_| panic!("Invalid core id: '{}'", spec));
            selected_ids.push(id);
        }
    }

    let mut seen = HashSet::new();
    for &id in &selected_ids {
        if !seen.insert(id) {
            panic!("Duplicate core id: {}", id);
        }
    }

    for &id in &selected_ids {
        if !valid_ids.contains(&id) {
            let mut available: Vec<usize> = valid_ids.iter().copied().collect();
            available.sort();
            panic!("Core {} not found. Available: {:?}", id, available);
        }
    }

    if selected_ids.len() < 2 {
        panic!("--cores must specify at least 2 cores, got {}", selected_ids.len());
    }

    selected_ids.iter()
        .map(|&id| *all_cores.iter().find(|c| c.id == id).unwrap())
        .collect()
}

fn main() {
    let args = CliArgs::parse();

    let all_cores = core_affinity::get_core_ids().expect("get_core_ids() failed");
    let cores = parse_cores(&args.cores, &all_cores);

    utils::show_cpuid_info();
    eprintln!("Num cores: {}", cores.len());
    eprintln!("Num iterations per samples: {}", args.num_iterations);
    eprintln!("Num samples: {}", args.num_samples);

    if args.num_addresses == 0 {
        eprintln!("ERROR: --num-addresses must be at least 1");
        std::process::exit(1);
    }

    #[cfg(target_os = "macos")]
    eprintln!("{}", ansi_term::Color::Red.bold().paint("WARN macOS may ignore thread-CPU affinity (we can't select a CPU to run on). Results may be inaccurate"));

    let clock = Arc::new(Clock::new());

    for b in &args.bench {
        match b {
            1 => {
                eprintln!();
                eprintln!("1) CAS latency on a single shared cache line");
                eprintln!();
                run_bench(&cores, &clock, &args, bench::cas::Bench::new(args.num_addresses), "cas");
            }
            2 => {
                eprintln!();
                eprintln!("2) Single-writer single-reader latency on two shared cache lines");
                eprintln!();
                run_bench(&cores, &clock, &args, bench::read_write::Bench::new(args.num_addresses), "read_write");
            }
            3 => {
                utils::assert_rdtsc_usable(&clock);
                eprintln!();
                eprintln!("3) Message passing. One writer and one reader on many cache line");
                eprintln!();
                run_bench(&cores, &clock, &args, bench::msg_passing::Bench::new(args.num_iterations, args.num_addresses), "msg_passing");
            }
            _ => panic!("--bench should be 1, 2 or 3"),
        }
    }
}
