pub mod cas;
pub mod read_write;
pub mod msg_passing;

use ansi_term::Color;
use core_affinity::CoreId;
use quanta::Clock;
use std::io::Write;
use crate::CliArgs;

pub type Count = u32;

/// Statistics computed from a sample of values -- shared by all benchmarks
pub struct Stats {
    pub mean: f64,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub cv_percent: f64,
}

impl Stats {
    /// Compute statistics from a slice of f64 values.
    /// Returns None if the slice is empty or contains only NaN values.
    pub fn compute(values: &[f64]) -> Option<Self> {
        let mut valid: Vec<f64> = values.iter().copied().filter(|v| !v.is_nan()).collect();
        if valid.is_empty() {
            return None;
        }

        let arr = ndarray::arr1(&valid);
        let mean = arr.mean().unwrap();
        // std(1.0) uses ddof=1 (sample std dev), which divides by n-1.
        // When n==1, this produces NaN. Treat cv_percent as 0.0 in that case.
        let cv_percent = if valid.len() <= 1 {
            0.0
        } else {
            let stddev = arr.std(1.0);
            if mean > 0.0 { stddev / mean * 100.0 } else { 0.0 }
        };

        let min = *valid.iter().min_by(|a, b| a.partial_cmp(b).unwrap()).unwrap();
        let max = *valid.iter().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap();

        valid.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = if valid.len() % 2 == 0 {
            (valid[valid.len() / 2 - 1] + valid[valid.len() / 2]) / 2.0
        } else {
            valid[valid.len() / 2]
        };

        Some(Self { mean, median, min, max, cv_percent })
    }
}

/// Helper module for CSV file creation
pub mod csv_output {
    use std::fs::File;
    use std::io::BufWriter;

    /// Create a CSV file with standard naming: <prefix>.<bench_name>.<suffix>.csv
    ///
    /// Returns None and prints error if file creation fails.
    pub fn create_csv(prefix: &str, bench_name: &str, suffix: &str) -> Option<BufWriter<File>> {
        let path = format!("{}.{}.{}.csv", prefix, bench_name, suffix);

        match File::create(&path) {
            Ok(file) => {
                eprintln!("    Writing CSV: {}", path);
                Some(BufWriter::new(file))
            }
            Err(e) => {
                eprintln!("    ERROR: Failed to create {}: {}", path, e);
                None
            }
        }
    }
}

pub trait Bench {
    fn run(&self, args: &CliArgs, cores: (CoreId, CoreId), clock: &Clock, num_iterations: Count, num_samples: Count) -> Vec<Vec<f64>>;
    /// Whether the bench on (i,j) is the same as the bench on (j,i)
    fn is_symmetric(&self) -> bool { true }
    /// Number of addresses this benchmark tests
    fn num_addresses(&self) -> usize { 1 }
    /// Get the virtual addresses of the storage for each address index.
    /// Returns empty vec if not applicable.
    fn address_ptrs(&self) -> Vec<usize> { vec![] }
    /// Get the NUMA node where memory is allocated.
    /// Returns 0 if NUMA is not applicable or not tracked.
    fn mem_numa(&self) -> usize { 0 }
}

/// Per-address statistics for a single core pair
struct PerAddressRow {
    ping_core: usize,
    pong_core: usize,
    ping_numa: usize,      // NUMA node of ping core
    pong_numa: usize,      // NUMA node of pong core
    mem_numa: usize,       // NUMA node where memory is allocated
    address: usize,
    vaddr: usize,
    mean: f64,
    median: f64,
    min: f64,
    max: f64,
    cv_percent: f64,
}

/// Per-pair summary (aggregated across addresses)
struct PerPairSummary {
    ping_core: usize,
    pong_core: usize,
    ping_numa: usize,      // NUMA node of ping core
    pong_numa: usize,      // NUMA node of pong core
    mem_numa: usize,       // NUMA node where memory is allocated
    mean_of_means: f64,
    mean_of_medians: f64,
    min: f64,
    max: f64,
    cv_percent: f64,
}

pub fn run_bench(cores: &[CoreId], clock: &Clock, args: &CliArgs, bench: impl Bench, bench_name: &str) {
    let num_samples = args.num_samples;
    let num_iterations = args.num_iterations;
    let num_addresses = bench.num_addresses();
    let address_ptrs = bench.address_ptrs();

    let n_cores = cores.len();
    assert!(n_cores >= 2);

    // Running stats for overall summary (streaming approach to reduce memory)
    let mut running_sum = 0.0;
    let mut running_count = 0u64;

    // Track min/max for both mean-of-means and mean-of-medians
    let mut min_latency_by_mean: Option<(f64, usize, usize)> = None;
    let mut max_latency_by_mean: Option<(f64, usize, usize)> = None;
    let mut min_latency_by_median: Option<(f64, usize, usize)> = None;
    let mut max_latency_by_median: Option<(f64, usize, usize)> = None;

    // Collect rows for CSV output. Only pre-allocate when CSV is requested.
    let n_pairs = if bench.is_symmetric() { n_cores * (n_cores - 1) / 2 } else { n_cores * (n_cores - 1) };
    let mut per_address_rows: Vec<PerAddressRow> = if args.csv {
        Vec::with_capacity(n_pairs * num_addresses)
    } else {
        Vec::new()
    };
    let mut per_pair_summaries: Vec<PerPairSummary> = if args.csv {
        Vec::with_capacity(n_pairs)
    } else {
        Vec::new()
    };

    // Legacy N x N matrix for stdout CSV output (mean-of-means).
    // Only allocated when CSV output is requested.
    let mut legacy_matrix: Vec<Vec<f64>> = if args.csv {
        vec![vec![f64::NAN; n_cores]; n_cores]
    } else {
        Vec::new()
    };

    // First print the column header
    eprint!("    {: >3}", "");
    for j in cores {
        eprint!(" {: >4}{: >4}", j.id, "");
    }
    eprintln!();

    let mcolor = Color::White.bold();
    let scolor = Color::White.dimmed();

    let mut pair_means = Vec::with_capacity(num_addresses);
    let mut pair_medians = Vec::with_capacity(num_addresses);
    let mut all_samples = vec![0.0; num_samples as usize];

    // Do the benchmark
    for i in 0..n_cores {
        let core_i = cores[i];
        eprint!("    {: >3}", core_i.id);
        for j in 0..n_cores {
            if bench.is_symmetric() {
                if i <= j {
                   continue;
                }
            } else if i == j {
                eprint!("{: >9}", "");
                continue;
            }

            let core_j = cores[j];
            // We add 1 warmup cycle first
            let durations = bench.run(args, (core_i, core_j), clock, num_iterations, 1+num_samples);

            pair_means.clear();
            pair_medians.clear();
            all_samples.fill(0.0);

            for addr_idx in 0..num_addresses {
                // Skip the first (warmup) sample
                let samples: &[f64] = &durations[addr_idx][1..];

                // Update running totals for overall summary
                for &val in samples {
                    running_sum += val;
                    running_count += 1;
                }

                // Accumulate for per-pair display (average across addresses)
                for (s, &val) in samples.iter().enumerate() {
                    all_samples[s] += val / num_addresses as f64;
                }

                // Compute per-address statistics
                if let Some(stats) = Stats::compute(samples) {
                    pair_means.push(stats.mean);
                    pair_medians.push(stats.median);

                    if args.csv {
                        let vaddr = if addr_idx < address_ptrs.len() {
                            address_ptrs[addr_idx]
                        } else {
                            0
                        };
                        per_address_rows.push(PerAddressRow {
                            ping_core: cores[i].id,
                            pong_core: cores[j].id,
                            ping_numa: 0,
                            pong_numa: 0,
                            mem_numa: bench.mem_numa(),
                            address: addr_idx,
                            vaddr,
                            mean: stats.mean,
                            median: stats.median,
                            min: stats.min,
                            max: stats.max,
                            cv_percent: stats.cv_percent,
                        });
                    }
                }
            }

            // Compute per-pair summary (aggregated across addresses)
            if !pair_means.is_empty() && !pair_medians.is_empty() {
                let mean_of_means = pair_means.iter().sum::<f64>() / pair_means.len() as f64;
                let mean_of_medians = pair_medians.iter().sum::<f64>() / pair_medians.len() as f64;

                // Track min/max by mean-of-means
                match &min_latency_by_mean {
                    None => min_latency_by_mean = Some((mean_of_means, i, j)),
                    Some((min_val, _, _)) if mean_of_means < *min_val => min_latency_by_mean = Some((mean_of_means, i, j)),
                    _ => {}
                }
                match &max_latency_by_mean {
                    None => max_latency_by_mean = Some((mean_of_means, i, j)),
                    Some((max_val, _, _)) if mean_of_means > *max_val => max_latency_by_mean = Some((mean_of_means, i, j)),
                    _ => {}
                }

                // Track min/max by mean-of-medians
                match &min_latency_by_median {
                    None => min_latency_by_median = Some((mean_of_medians, i, j)),
                    Some((min_val, _, _)) if mean_of_medians < *min_val => min_latency_by_median = Some((mean_of_medians, i, j)),
                    _ => {}
                }
                match &max_latency_by_median {
                    None => max_latency_by_median = Some((mean_of_medians, i, j)),
                    Some((max_val, _, _)) if mean_of_medians > *max_val => max_latency_by_median = Some((mean_of_medians, i, j)),
                    _ => {}
                }

                if args.csv {
                    legacy_matrix[i][j] = mean_of_means;
                    if bench.is_symmetric() {
                        legacy_matrix[j][i] = mean_of_means;
                    }
                }

                if args.csv {
                    // For the summary rows, compute stats from means
                    if let Some(stats) = Stats::compute(&pair_means) {
                        per_pair_summaries.push(PerPairSummary {
                            ping_core: cores[i].id,
                            pong_core: cores[j].id,
                            ping_numa: 0,
                            pong_numa: 0,
                            mem_numa: bench.mem_numa(),
                            mean_of_means,
                            mean_of_medians,
                            min: stats.min,
                            max: stats.max,
                            cv_percent: stats.cv_percent,
                        });
                    }
                }
            }

            // Display matrix entry
            let all_samples_arr = ndarray::arr1(&all_samples);
            let mean = format!("{: >4.0}", all_samples_arr.mean().unwrap());
            let stddev = format!("+-{: <2.0}", all_samples_arr.std(1.0).min(99.0) / (num_samples as f64).sqrt());
            eprint!(" {}{}", mcolor.paint(mean), scolor.paint(stddev));
            let _ = std::io::stderr().lock().flush();
        }
        eprintln!();
    }

    eprintln!();

    // Print min/max latency (using median-based, which is more robust)
    eprintln!("    {} (robust to outliers):", scolor.paint("Median-based stats"));
    if let Some((min_val, min_i, min_j)) = min_latency_by_median {
        eprintln!("      Min  latency: {}ns cores: ({},{})",
                 mcolor.paint(format!("{:.1}", min_val)), cores[min_i].id, cores[min_j].id);
    }
    if let Some((max_val, max_i, max_j)) = max_latency_by_median {
        eprintln!("      Max  latency: {}ns cores: ({},{})",
                 mcolor.paint(format!("{:.1}", max_val)), cores[max_i].id, cores[max_j].id);
    }

    eprintln!("    {} (sensitive to outliers):", scolor.paint("Mean-based stats"));
    if let Some((min_val, min_i, min_j)) = min_latency_by_mean {
        eprintln!("      Min  latency: {}ns cores: ({},{})",
                 mcolor.paint(format!("{:.1}", min_val)), cores[min_i].id, cores[min_j].id);
    }
    if let Some((max_val, max_i, max_j)) = max_latency_by_mean {
        eprintln!("      Max  latency: {}ns cores: ({},{})",
                 mcolor.paint(format!("{:.1}", max_val)), cores[max_i].id, cores[max_j].id);
    }

    // Print overall mean latency (computed from running totals)
    if running_count > 0 {
        let overall_mean = running_sum / running_count as f64;
        eprintln!("    Overall mean latency: {}ns", mcolor.paint(format!("{:.1}", overall_mean)));
    }

    // Write CSV files if requested
    if args.csv {
        write_csv_files(&args.csv_output_prefix, bench_name, &per_address_rows, &per_pair_summaries);

        // Print labeled N x N matrix CSV to stdout (enhanced format with
        // core ID header row and row labels, values at 1 decimal place).
        // Header row: empty cell, then core IDs
        print!(",");
        for core in cores {
            print!("{}", core.id);
            if core.id != cores.last().unwrap().id {
                print!(",");
            }
        }
        println!();

        // Data rows: core ID, then latency values
        for i in 0..n_cores {
            print!("{}", cores[i].id);
            for j in 0..n_cores {
                print!(",");
                let val = legacy_matrix[i][j];
                if !val.is_nan() {
                    print!("{:.1}", val);
                }
            }
            println!();
        }
    }
}

fn write_csv_files(prefix: &str, bench_name: &str, per_address_rows: &[PerAddressRow], per_pair_summaries: &[PerPairSummary]) {
    // Write per-address CSV
    if let Some(mut writer) = csv_output::create_csv(prefix, bench_name, "per_address") {
        writeln!(writer, "ping_core,pong_core,ping_numa,pong_numa,mem_numa,address,vaddr,mean_latency,median_latency,min_latency,max_latency,cv_percent").unwrap();
        for row in per_address_rows {
            writeln!(writer, "{},{},{},{},{},{},0x{:x},{:.2},{:.2},{:.2},{:.2},{:.2}",
                    row.ping_core, row.pong_core, row.ping_numa, row.pong_numa, row.mem_numa,
                    row.address, row.vaddr,
                    row.mean, row.median, row.min, row.max, row.cv_percent).unwrap();
        }
    }

    // Write combined summary CSV (includes both mean-of-means and mean-of-medians)
    if let Some(mut writer) = csv_output::create_csv(prefix, bench_name, "summary") {
        writeln!(writer, "ping_core,pong_core,ping_numa,pong_numa,mem_numa,mean_of_means,mean_of_medians,min_latency,max_latency,cv_percent").unwrap();
        for row in per_pair_summaries {
            writeln!(writer, "{},{},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2}",
                    row.ping_core, row.pong_core, row.ping_numa, row.pong_numa, row.mem_numa,
                    row.mean_of_means, row.mean_of_medians,
                    row.min, row.max, row.cv_percent).unwrap();
        }
    }
}
