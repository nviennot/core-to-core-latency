mod bench;
mod numa;
mod utils;

use bench::Count;
use bench::SampleAgg;
use numa::MemPlacement;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use clap::Parser;
use core_affinity::CoreId;
use quanta::Clock;
use crate::bench::run_bench;

/// CSV schema documentation embedded in `--help` (also mirrored in the README).
const CSV_HELP: &str = "\
CSV OUTPUT (--csv)

Glossary:
  cell      The finest measured unit: a unique (mem_numa, ping_core, pong_core,
            address). Each cell is measured num_samples times.
  sample-agg-collapsed value
            A cell's num_samples raw measurements reduced to one number by
            --sample_agg (mean or median; default median). It equals that cell's
            mean_latency (--sample_agg mean) or median_latency (--sample_agg
            median) as reported in per_address.csv.

NUMA columns:
  mem_numa  The NUMA node the shared buffer physically landed on, confirmed via
            move_pages. mem_numa = -1 means unverified (the kernel could not
            report placement on this host).
  ping_numa / pong_numa
            The NUMA node of the ping / pong core (from sysfs).

Files written per benchmark:

  file              one row per                              emitted     the five stats are computed over...
  ----              -----------                              -------     -----------------------------------
  per_address.csv   cell: (mem_numa,ping,pong,address)       always      that cell's num_samples raw measurements
  per_domain.csv    (ping_core,pong_core,mem_numa)           >1 domain   the sample-agg-collapsed values of all
                                                                         addresses in that (pair, domain)
  per_pair.csv      core pair: (ping_core,pong_core)         always      the sample-agg-collapsed values of all
                                                                         addresses across all domains for that pair

Columns (exact header order):
  per_address.csv:
    ping_core,pong_core,ping_numa,pong_numa,mem_numa,address,vaddr,mean_latency,median_latency,min_latency,max_latency,cv_percent
  per_domain.csv:
    ping_core,pong_core,ping_numa,pong_numa,mem_numa,mean_latency,median_latency,min_latency,max_latency,cv_percent
  per_pair.csv:
    ping_core,pong_core,ping_numa,pong_numa,mean_latency,median_latency,min_latency,max_latency,cv_percent";

const DEFAULT_NUM_SAMPLES: Count = 300;
const DEFAULT_NUM_ITERATIONS_PER_SAMPLE: Count = 1000;
const DEFAULT_NUM_ADDRESSES: usize = 1;

#[derive(Clone)]
#[derive(clap::Parser)]
#[clap(after_long_help = CSV_HELP)]
pub struct CliArgs {
    /// The number of iterations per sample
    #[clap(default_value_t = DEFAULT_NUM_ITERATIONS_PER_SAMPLE, value_parser)]
    num_iterations: Count,

    /// The number of samples
    #[clap(default_value_t = DEFAULT_NUM_SAMPLES, value_parser)]
    num_samples: Count,

    /// The number of addresses to test per NUMA domain (each address is a {n}
    /// separate cacheline). One buffer of this many cache lines is allocated {n}
    /// for every domain selected by --numa. Only the CAS bench (1) uses >1.
    #[clap(long = "num_addresses", alias = "num-addresses", default_value_t = DEFAULT_NUM_ADDRESSES, value_parser)]
    pub num_addresses: usize,

    /// How to collapse a cell's num_samples to one value before the per-pair {n}
    /// summary aggregates across addresses (does not affect per_address.csv). {n}
    /// See the CSV section of --help (long help) for details.
    #[clap(long = "sample_agg", alias = "sample-agg", value_enum, default_value_t = SampleAgg::Median, value_parser)]
    pub sample_agg: SampleAgg,

    /// Output CSV files with latency data. When enabled, writes per benchmark: {n}
    ///   <prefix>.<bench>.per_address.csv - one row per cell, five stats over raw samples {n}
    ///   <prefix>.<bench>.per_domain.csv - one row per (pair, mem_numa); only when {n}
    ///   running more than one NUMA domain {n}
    ///   <prefix>.<bench>.per_pair.csv - one row per core pair, five stats over the {n}
    ///   per-address sample-agg-collapsed values across all domains. See --help (long help).
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

    /// NUMA memory domain(s) for the shared cache lines. Single id, comma list, {n}
    /// inclusive ranges, mixed, or "all". One buffer of num_addresses cache {n}
    /// lines is allocated PER domain, and the whole core matrix is benchmarked {n}
    /// once per domain. Requires the `numa` build feature (Linux only). {n}
    /// Examples: --numa 0   --numa 0,1   --numa 0-3   --numa 1,3-5   --numa all
    #[clap(long, require_delimiter=true, value_delimiter=',', value_parser)]
    numa: Vec<String>,
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

/// Parse the `--numa` spec list into one `MemPlacement` per requested domain.
///
/// Pure (takes `num_nodes` as a parameter) so it is unit-testable with no NUMA
/// hardware and no feature dependency. Mirrors `parse_cores`:
/// - empty specs            -> `[Default]` (kernel-default placement),
/// - `"all"`                -> `[Bound(0)..Bound(num_nodes-1)]`,
/// - ids and `a-b` ranges   -> `[Bound(..)]`, deduped and range-validated.
///
/// When the binary was built without the `numa` feature, any explicit request
/// is rejected with a "rebuild with --features numa" message.
fn parse_numa(specs: &[String], num_nodes: usize) -> Result<Vec<MemPlacement>, String> {
    if specs.is_empty() {
        return Ok(vec![MemPlacement::Default]);
    }

    if !numa::feature_enabled() {
        return Err("--numa requires the `numa` build feature; rebuild with \
                    `cargo build --release --features numa` (Linux only)".to_string());
    }

    parse_numa_specs(specs, num_nodes)
}

/// Pure spec parser shared by `parse_numa`. Does the actual id/range parsing,
/// dedupe, and range validation, independent of the `numa` feature gate so it
/// is unit-testable on any machine. `specs` is assumed non-empty.
fn parse_numa_specs(specs: &[String], num_nodes: usize) -> Result<Vec<MemPlacement>, String> {
    // "all" expands to every configured node.
    if specs.len() == 1 && specs[0].trim() == "all" {
        return Ok((0..num_nodes).map(MemPlacement::Bound).collect());
    }

    let mut selected: Vec<usize> = Vec::new();
    for spec in specs {
        let spec = spec.trim();
        if spec == "all" {
            return Err("'all' cannot be combined with other --numa specs".to_string());
        }
        if spec.contains('-') {
            let parts: Vec<&str> = spec.splitn(2, '-').collect();
            let start: usize = parts[0].parse()
                .map_err(|_| format!("Invalid range start in '{}': '{}'", spec, parts[0]))?;
            let end: usize = parts[1].parse()
                .map_err(|_| format!("Invalid range end in '{}': '{}'", spec, parts[1]))?;
            if start > end {
                return Err(format!("Invalid range '{}': start ({}) > end ({})", spec, start, end));
            }
            for id in start..=end {
                selected.push(id);
            }
        } else {
            let id: usize = spec.parse()
                .map_err(|_| format!("Invalid NUMA domain id: '{}'", spec))?;
            selected.push(id);
        }
    }

    let mut seen = HashSet::new();
    for &id in &selected {
        if !seen.insert(id) {
            return Err(format!("Duplicate NUMA domain id: {}", id));
        }
    }

    for &id in &selected {
        if id >= num_nodes {
            return Err(format!(
                "NUMA domain {} not found; system has {} node(s) (0..{})",
                id, num_nodes, num_nodes.saturating_sub(1)
            ));
        }
    }

    Ok(selected.into_iter().map(MemPlacement::Bound).collect())
}

/// Compress a sorted cpu list into "a-b,c" ranges for display.
fn fmt_ranges(cpus: &[usize]) -> String {
    if cpus.is_empty() {
        return String::new();
    }
    let mut v = cpus.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut parts = Vec::new();
    let mut start = v[0];
    let mut prev = v[0];
    for &c in &v[1..] {
        if c == prev + 1 {
            prev = c;
        } else {
            if start == prev {
                parts.push(start.to_string());
            } else {
                parts.push(format!("{}-{}", start, prev));
            }
            start = c;
            prev = c;
        }
    }
    if start == prev {
        parts.push(start.to_string());
    } else {
        parts.push(format!("{}-{}", start, prev));
    }
    parts.join(",")
}

fn fmt_domains(placements: &[numa::MemPlacement]) -> String {
    if placements.iter().all(|p| matches!(p, numa::MemPlacement::Default)) {
        return "default".to_string();
    }
    let ids: Vec<String> = placements
        .iter()
        .map(|p| match p {
            numa::MemPlacement::Default => "default".to_string(),
            numa::MemPlacement::Bound(n) => n.to_string(),
        })
        .collect();
    ids.join(",")
}

/// Clean exit on a CLI / allocation error, mirroring the other ERROR paths.
fn die(e: impl std::fmt::Display) -> ! {
    eprintln!("ERROR: {}", e);
    std::process::exit(1);
}

fn main() {
    let args = CliArgs::parse();

    numa::init();

    let all_cores = core_affinity::get_core_ids().expect("get_core_ids() failed");
    let cores = parse_cores(&args.cores, &all_cores);

    let num_nodes = numa::num_nodes();
    let placements = parse_numa(&args.numa, num_nodes).unwrap_or_else(|e| die(e));

    // Core -> NUMA node map for the selected cores (sysfs; always >= 0).
    let core_numa: HashMap<usize, i32> = cores
        .iter()
        .map(|c| (c.id, numa::node_of_cpu(c.id) as i32))
        .collect();

    utils::show_cpuid_info();
    eprintln!("Num cores: {}", cores.len());

    let summary = numa::summary_info();

    // Centralized NUMA reporting: main formats all user-facing messages, keyed
    // on whether the host actually has more than one NUMA node.
    if summary.num_nodes <= 1 {
        // Single-domain host: one informational line and no warnings,
        // regardless of whether the `numa` feature was compiled in.
        eprintln!("NUMA: not detected (single domain)");
    } else if !summary.feature_enabled {
        // NUMA hardware, but this binary cannot place memory.
        eprintln!(
            "NUMA: {} nodes detected; binary built without the `numa` feature",
            summary.num_nodes
        );
        eprintln!("{}", ansi_term::Color::Yellow.bold().paint(format!(
            "WARN: {} NUMA nodes detected but this binary was not built with the \
             'numa' feature. Rebuild with `cargo build --release --features numa` \
             (Linux only) and pass --numa <node(s)> to pin placement. Running with \
             kernel-default first-touch placement, which may vary run-to-run.",
            summary.num_nodes
        )));
    } else {
        // NUMA hardware with placement support: print the topology.
        let topo = summary.topology.as_ref().expect("topology present when feature enabled");
        eprintln!("NUMA: {} node(s)", topo.nodes.len());
        for ni in &topo.nodes {
            let gb = ni.mem_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            let mem = if ni.mem_bytes == 0 {
                "? GB".to_string()
            } else {
                format!("{:.1} GB", gb)
            };
            eprintln!(
                "  node {}: {} cpus [{}], {}",
                ni.node,
                ni.cpus.len(),
                fmt_ranges(&ni.cpus),
                mem
            );
        }
        eprintln!("  Memory domains under test: {}", fmt_domains(&placements));

        // Advisory when run on NUMA hardware without pinning placement: the
        // shared buffer uses kernel-default first-touch, so results may vary
        // run-to-run. If the buffer actually lands split across nodes, run_bench
        // emits a second, louder warning once placement is known.
        if args.numa.is_empty() {
            eprintln!("{}", ansi_term::Color::Yellow.bold().paint(format!(
                "WARN: {} NUMA nodes detected but --numa was not given. The shared cache \
                 lines use kernel-default first-touch placement, so results may vary \
                 run-to-run. Pass --numa <node(s)> to pin placement.",
                summary.num_nodes
            )));
        }
    }

    eprintln!("Num iterations per samples: {}", args.num_iterations);
    eprintln!("Num samples: {}", args.num_samples);
    eprintln!("Sample aggregation: {}", match args.sample_agg {
        SampleAgg::Mean => "mean",
        SampleAgg::Median => "median",
    });

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
                let bench = bench::cas::Bench::new(args.num_addresses, &placements).unwrap_or_else(|e| die(e));
                run_bench(&cores, &core_numa, &clock, &args, bench, "cas");
            }
            2 => {
                eprintln!();
                eprintln!("2) Single-writer single-reader latency on two shared cache lines");
                eprintln!();
                let bench = bench::read_write::Bench::new(args.num_addresses, &placements).unwrap_or_else(|e| die(e));
                run_bench(&cores, &core_numa, &clock, &args, bench, "read_write");
            }
            3 => {
                utils::assert_rdtsc_usable(&clock);
                eprintln!();
                eprintln!("3) Message passing. One writer and one reader on many cache line");
                eprintln!();
                let bench = bench::msg_passing::Bench::new(args.num_iterations, args.num_addresses, &placements).unwrap_or_else(|e| die(e));
                run_bench(&cores, &core_numa, &clock, &args, bench, "msg_passing");
            }
            _ => panic!("--bench should be 1, 2 or 3"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use numa::MemPlacement::{Bound, Default as Def};

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    // These exercise the pure spec parser, which runs on any machine with no
    // NUMA hardware and independent of the `numa` feature.

    #[test]
    fn empty_specs_is_default() {
        assert_eq!(parse_numa(&[], 4).unwrap(), vec![Def]);
    }

    #[test]
    fn single_id() {
        assert_eq!(parse_numa_specs(&s(&["1"]), 4).unwrap(), vec![Bound(1)]);
    }

    #[test]
    fn comma_list() {
        assert_eq!(parse_numa_specs(&s(&["0", "2"]), 4).unwrap(), vec![Bound(0), Bound(2)]);
    }

    #[test]
    fn range() {
        assert_eq!(
            parse_numa_specs(&s(&["0-3"]), 4).unwrap(),
            vec![Bound(0), Bound(1), Bound(2), Bound(3)]
        );
    }

    #[test]
    fn mixed() {
        assert_eq!(
            parse_numa_specs(&s(&["1", "3-5"]), 8).unwrap(),
            vec![Bound(1), Bound(3), Bound(4), Bound(5)]
        );
    }

    #[test]
    fn all_expands() {
        assert_eq!(parse_numa_specs(&s(&["all"]), 2).unwrap(), vec![Bound(0), Bound(1)]);
    }

    #[test]
    fn all_cannot_combine() {
        assert!(parse_numa_specs(&s(&["all", "1"]), 4).is_err());
    }

    #[test]
    fn duplicate_errors() {
        assert!(parse_numa_specs(&s(&["0", "0"]), 4).is_err());
    }

    #[test]
    fn reversed_range_errors() {
        assert!(parse_numa_specs(&s(&["3-1"]), 4).is_err());
    }

    #[test]
    fn non_numeric_errors() {
        assert!(parse_numa_specs(&s(&["x"]), 4).is_err());
    }

    #[test]
    fn out_of_range_mentions_node_count() {
        let err = parse_numa_specs(&s(&["7"]), 2).unwrap_err();
        assert!(err.contains("2 node(s)"), "error was: {err}");
    }

    #[test]
    fn feature_off_rejects_explicit_numa() {
        // The default test build has the `numa` feature off, so any explicit
        // --numa request is rejected with a rebuild hint. (When run with
        // --features numa this assertion is skipped since parsing succeeds.)
        if !numa::feature_enabled() {
            let err = parse_numa(&s(&["0"]), 4).unwrap_err();
            assert!(err.contains("--features numa"), "error was: {err}");
        }
    }
}
