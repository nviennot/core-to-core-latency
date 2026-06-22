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
    fn run(&self, cores: (CoreId, CoreId), clock: &Clock, num_iterations: Count, num_samples: Count) -> Vec<Vec<f64>>;
    /// Whether the bench on (i,j) is the same as the bench on (j,i)
    fn is_symmetric(&self) -> bool { true }
    /// Number of addresses this benchmark tests
    fn num_addresses(&self) -> usize { 1 }
    /// Get the virtual addresses of the storage for each address index.
    /// Returns empty vec if not applicable.
    fn address_ptrs(&self) -> Vec<usize> { vec![] }
}

/// How a cell's num_samples raw measurements collapse to one representative value
/// before the per-pair summary aggregates across addresses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum SampleAgg {
    Mean,
    Median,
}

impl SampleAgg {
    /// Collapse a cell's raw-sample statistics to one representative value.
    fn collapse(self, stats: &Stats) -> f64 {
        match self {
            SampleAgg::Mean => stats.mean,
            SampleAgg::Median => stats.median,
        }
    }
}

/// Per-address statistics for a single core pair (one row per cell).
/// The five stats are computed over the cell's raw samples.
struct PerAddressRow {
    ping_core: usize,
    pong_core: usize,
    address: usize,
    vaddr: usize,
    mean: f64,
    median: f64,
    min: f64,
    max: f64,
    cv_percent: f64,
}

/// Per-pair summary (aggregated across addresses).
/// The five stats are computed over the sample-agg-collapsed values of the pair's cells.
struct PerPairSummary {
    ping_core: usize,
    pong_core: usize,
    mean: f64,
    median: f64,
    min: f64,
    max: f64,
    cv_percent: f64,
}

pub fn run_bench(cores: &[CoreId], clock: &Clock, args: &CliArgs, bench: impl Bench, bench_name: &str) {
    let num_samples = args.num_samples;
    let num_iterations = args.num_iterations;
    let num_addresses = bench.num_addresses();
    let address_ptrs = bench.address_ptrs();
    let sample_agg = args.sample_agg;

    let n_cores = cores.len();
    assert!(n_cores >= 2);

    // Running stats for overall summary (streaming approach to reduce memory)
    let mut running_sum = 0.0;
    let mut running_count = 0u64;

    // Track min/max of the per-pair summary mean and median across all pairs.
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

    // Sample-agg-collapsed value (one per address/cell) for the current pair.
    let mut pair_collapsed = Vec::with_capacity(num_addresses);
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
            let durations = bench.run((core_i, core_j), clock, num_iterations, 1+num_samples);

            pair_collapsed.clear();
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

                // Per-address statistics: five stats over this cell's raw samples.
                if let Some(stats) = Stats::compute(samples) {
                    // The cell's sample-agg-collapsed value.
                    pair_collapsed.push(sample_agg.collapse(&stats));

                    if args.csv {
                        let vaddr = if addr_idx < address_ptrs.len() {
                            address_ptrs[addr_idx]
                        } else {
                            0
                        };
                        per_address_rows.push(PerAddressRow {
                            ping_core: cores[i].id,
                            pong_core: cores[j].id,
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

            // Per-pair summary: single Stats::compute over the sample-agg-collapsed values.
            if let Some(stats) = Stats::compute(&pair_collapsed) {
                // Track min/max of the per-pair mean across all pairs.
                match &min_latency_by_mean {
                    None => min_latency_by_mean = Some((stats.mean, i, j)),
                    Some((min_val, _, _)) if stats.mean < *min_val => min_latency_by_mean = Some((stats.mean, i, j)),
                    _ => {}
                }
                match &max_latency_by_mean {
                    None => max_latency_by_mean = Some((stats.mean, i, j)),
                    Some((max_val, _, _)) if stats.mean > *max_val => max_latency_by_mean = Some((stats.mean, i, j)),
                    _ => {}
                }

                // Track min/max of the per-pair median across all pairs.
                match &min_latency_by_median {
                    None => min_latency_by_median = Some((stats.median, i, j)),
                    Some((min_val, _, _)) if stats.median < *min_val => min_latency_by_median = Some((stats.median, i, j)),
                    _ => {}
                }
                match &max_latency_by_median {
                    None => max_latency_by_median = Some((stats.median, i, j)),
                    Some((max_val, _, _)) if stats.median > *max_val => max_latency_by_median = Some((stats.median, i, j)),
                    _ => {}
                }

                if args.csv {
                    // Feed the legacy matrix cell the per-pair mean of the collapsed values.
                    legacy_matrix[i][j] = stats.mean;
                    if bench.is_symmetric() {
                        legacy_matrix[j][i] = stats.mean;
                    }

                    per_pair_summaries.push(PerPairSummary {
                        ping_core: cores[i].id,
                        pong_core: cores[j].id,
                        mean: stats.mean,
                        median: stats.median,
                        min: stats.min,
                        max: stats.max,
                        cv_percent: stats.cv_percent,
                    });
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
    // Write per-address CSV (one row per cell; five stats over raw samples)
    if let Some(mut writer) = csv_output::create_csv(prefix, bench_name, "per_address") {
        writeln!(writer, "ping_core,pong_core,address,vaddr,mean_latency,median_latency,min_latency,max_latency,cv_percent").unwrap();
        for row in per_address_rows {
            writeln!(writer, "{},{},{},0x{:x},{:.2},{:.2},{:.2},{:.2},{:.2}",
                    row.ping_core, row.pong_core,
                    row.address, row.vaddr,
                    row.mean, row.median, row.min, row.max, row.cv_percent).unwrap();
        }
    }

    // Write per-pair CSV (five stats over the sample-agg-collapsed values of the pair)
    if let Some(mut writer) = csv_output::create_csv(prefix, bench_name, "per_pair") {
        writeln!(writer, "ping_core,pong_core,mean_latency,median_latency,min_latency,max_latency,cv_percent").unwrap();
        for row in per_pair_summaries {
            writeln!(writer, "{},{},{:.2},{:.2},{:.2},{:.2},{:.2}",
                    row.ping_core, row.pong_core,
                    row.mean, row.median, row.min, row.max, row.cv_percent).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror the production two-stage per-pair aggregation: for each cell,
    /// compute Stats over its raw samples and collapse to one value via
    /// `agg`; then run a single Stats::compute over the collapsed values.
    fn summarize_pair(cells: &[Vec<f64>], agg: SampleAgg) -> Stats {
        let collapsed: Vec<f64> = cells
            .iter()
            .filter_map(|samples| Stats::compute(samples).map(|s| agg.collapse(&s)))
            .collect();
        Stats::compute(&collapsed).expect("non-empty pair")
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {a} ~= {b}");
    }

    #[test]
    fn collapse_selects_mean_or_median() {
        // Raw samples whose mean and median differ.
        let stats = Stats::compute(&[10.0, 10.0, 40.0]).unwrap();
        approx(stats.mean, 20.0);
        approx(stats.median, 10.0);
        approx(SampleAgg::Mean.collapse(&stats), 20.0);
        approx(SampleAgg::Median.collapse(&stats), 10.0);
    }

    #[test]
    fn per_pair_equals_stats_over_collapsed_both_modes() {
        // Two cells whose per-cell mean and median differ, so the two modes
        // produce genuinely different collapsed sets (not a tautology).
        let cells = vec![vec![10.0, 10.0, 40.0], vec![20.0, 80.0, 80.0]];

        // Mean mode: collapsed = [20, 60].
        let mean_collapsed = vec![20.0, 60.0];
        let expected_mean = Stats::compute(&mean_collapsed).unwrap();
        let got_mean = summarize_pair(&cells, SampleAgg::Mean);
        approx(got_mean.mean, expected_mean.mean);
        approx(got_mean.median, expected_mean.median);
        approx(got_mean.min, expected_mean.min);
        approx(got_mean.max, expected_mean.max);
        approx(got_mean.cv_percent, expected_mean.cv_percent);

        // Median mode: collapsed = [10, 80].
        let median_collapsed = vec![10.0, 80.0];
        let expected_median = Stats::compute(&median_collapsed).unwrap();
        let got_median = summarize_pair(&cells, SampleAgg::Median);
        approx(got_median.mean, expected_median.mean);
        approx(got_median.median, expected_median.median);
        approx(got_median.min, expected_median.min);
        approx(got_median.max, expected_median.max);
        approx(got_median.cv_percent, expected_median.cv_percent);

        // The two modes really diverge.
        assert!((got_mean.mean - got_median.mean).abs() > 1.0);
    }

    #[test]
    fn single_address_pair_is_degenerate() {
        // One cell -> collapsed has a single element -> min==mean==median==max, cv==0.
        let cells = vec![vec![10.0, 20.0, 30.0]];
        for agg in [SampleAgg::Mean, SampleAgg::Median] {
            let s = summarize_pair(&cells, agg);
            approx(s.cv_percent, 0.0);
            approx(s.min, s.mean);
            approx(s.mean, s.median);
            approx(s.median, s.max);
        }
    }

    #[test]
    fn two_address_pair_pinned_values_median_mode() {
        // Cell A median = 20, cell B median = 50 -> collapsed [20, 50].
        let cells = vec![vec![10.0, 20.0, 30.0], vec![40.0, 50.0, 60.0]];
        let s = summarize_pair(&cells, SampleAgg::Median);
        approx(s.mean, 35.0);
        approx(s.median, 35.0);
        approx(s.min, 20.0);
        approx(s.max, 50.0);
        // stddev (ddof=1) over [20,50] = sqrt(450) = 21.213203...; cv = /35*100.
        approx(s.cv_percent, 450.0_f64.sqrt() / 35.0 * 100.0);
    }

    #[test]
    fn two_address_pair_pinned_values_mean_mode() {
        // Cell A mean = 20, cell B mean = 60 -> collapsed [20, 60].
        let cells = vec![vec![10.0, 10.0, 40.0], vec![20.0, 80.0, 80.0]];
        let s = summarize_pair(&cells, SampleAgg::Mean);
        approx(s.mean, 40.0);
        approx(s.median, 40.0);
        approx(s.min, 20.0);
        approx(s.max, 60.0);
        // stddev (ddof=1) over [20,60] = sqrt(800) = 28.284271...; cv = /40*100.
        approx(s.cv_percent, 800.0_f64.sqrt() / 40.0 * 100.0);
    }
}
