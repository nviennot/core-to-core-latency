pub mod cas;
pub mod read_write;
pub mod msg_passing;

use ansi_term::Color;
use core_affinity::CoreId;
use quanta::Clock;
use std::collections::HashMap;
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
    /// Run the benchmark for one memory domain (`domain_idx`), using the shared
    /// buffer allocated for that domain.
    fn run(&self, domain_idx: usize, cores: (CoreId, CoreId), clock: &Clock, num_iterations: Count, num_samples: Count) -> Vec<Vec<f64>>;
    /// Whether the bench on (i,j) is the same as the bench on (j,i)
    fn is_symmetric(&self) -> bool { true }
    /// Number of addresses this benchmark tests
    fn num_addresses(&self) -> usize { 1 }
    /// Number of memory domains (one shared buffer per requested NUMA placement).
    fn num_domains(&self) -> usize { 1 }
    /// Get the virtual addresses of the storage for each address index, for the
    /// given memory domain. Returns empty vec if not applicable.
    fn address_ptrs(&self, _domain_idx: usize) -> Vec<usize> { vec![] }
    /// Resolved NUMA node for each memory domain's buffer, in placement order.
    /// `-1` means the placement could not be verified. Defaults to a single
    /// node 0 when NUMA is not tracked.
    fn mem_numa_nodes(&self) -> Vec<i32> { vec![0] }
    /// Whether each memory domain's buffer landed on more than one NUMA node,
    /// in placement order. Only possible under default first-touch placement;
    /// defaults to a single non-spanning domain when NUMA is not tracked.
    fn mem_numa_spanned(&self) -> Vec<bool> { vec![false] }
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
    ping_numa: i32,        // NUMA node of the ping core (sysfs)
    pong_numa: i32,        // NUMA node of the pong core (sysfs)
    mem_numa: i32,         // NUMA node the shared buffer landed on (-1 if unverified)
    address: usize,
    vaddr: usize,
    mean: f64,
    median: f64,
    min: f64,
    max: f64,
    cv_percent: f64,
}

/// One measured cell, reduced to its sample-agg-collapsed value plus the
/// grouping/metadata columns. The summary files are produced by a single
/// generic group-by over these records.
struct CellRecord {
    ping_core: usize,
    pong_core: usize,
    ping_numa: i32,        // NUMA node of the ping core (constant per pair)
    pong_numa: i32,        // NUMA node of the pong core (constant per pair)
    mem_numa: i32,         // NUMA node the shared buffer landed on (-1 if unverified)
    collapsed: f64,        // sample-agg-collapsed value for this cell
}

/// Group per-cell records by a key (preserving first-seen order) and compute
/// `Stats` over each group's sample-agg-collapsed values. Returns, for each
/// non-empty group, the index of its first record (so callers can read the
/// group's constant columns) and the `Stats` over the group.
fn group_cells<K, F>(records: &[CellRecord], key_of: F) -> Vec<(usize, Stats)>
where
    K: std::cmp::Eq + std::hash::Hash,
    F: Fn(&CellRecord) -> K,
{
    // (representative record index, collapsed values), in first-seen order.
    let mut groups: Vec<(usize, Vec<f64>)> = Vec::new();
    let mut index: HashMap<K, usize> = HashMap::new();
    for (idx, rec) in records.iter().enumerate() {
        let key = key_of(rec);
        match index.get(&key) {
            Some(&pos) => groups[pos].1.push(rec.collapsed),
            None => {
                index.insert(key, groups.len());
                groups.push((idx, vec![rec.collapsed]));
            }
        }
    }
    groups
        .into_iter()
        .filter_map(|(rep, vals)| Stats::compute(&vals).map(|s| (rep, s)))
        .collect()
}

/// Format a resolved memory-domain node for the console.
fn fmt_mem_numa(node: i32) -> String {
    if node < 0 {
        "memory placement unverified".to_string()
    } else {
        format!("NUMA node {}", node)
    }
}

pub fn run_bench(cores: &[CoreId], core_numa: &HashMap<usize, i32>, clock: &Clock, args: &CliArgs, bench: impl Bench, bench_name: &str) {
    let num_samples = args.num_samples;
    let num_iterations = args.num_iterations;
    let num_addresses = bench.num_addresses();
    let sample_agg = args.sample_agg;
    let num_domains = bench.num_domains();
    let mem_numa_nodes = bench.mem_numa_nodes();
    let mem_numa_spanned = bench.mem_numa_spanned();

    let n_cores = cores.len();
    assert!(n_cores >= 2);

    // Core id -> column index, for filling the legacy stdout matrix.
    let id_to_idx: HashMap<usize, usize> =
        cores.iter().enumerate().map(|(idx, c)| (c.id, idx)).collect();

    // Running stats for overall summary (streaming approach to reduce memory)
    let mut running_sum = 0.0;
    let mut running_count = 0u64;

    let n_pairs = if bench.is_symmetric() { n_cores * (n_cores - 1) / 2 } else { n_cores * (n_cores - 1) };

    // One record per measured cell, across every (domain, pair, address). Used
    // for both the console summary and the group-by CSV summaries.
    let mut cell_records: Vec<CellRecord> = Vec::with_capacity(n_pairs * num_addresses * num_domains);

    // Per-address rows are only needed for CSV.
    let mut per_address_rows: Vec<PerAddressRow> = if args.csv {
        Vec::with_capacity(n_pairs * num_addresses * num_domains)
    } else {
        Vec::new()
    };

    // Legacy N x N matrix for stdout CSV output (per-pair mean, flat across
    // domains). Only allocated when CSV output is requested.
    let mut legacy_matrix: Vec<Vec<f64>> = if args.csv {
        vec![vec![f64::NAN; n_cores]; n_cores]
    } else {
        Vec::new()
    };

    let mcolor = Color::White.bold();
    let scolor = Color::White.dimmed();

    // Sample-agg-collapsed value (one per address/cell) for the current pair.
    let mut pair_collapsed = Vec::with_capacity(num_addresses);
    let mut all_samples = vec![0.0; num_samples as usize];

    // Outer loop over memory domains; the per-pair sweep is run once per domain.
    for domain_idx in 0..num_domains {
        let mem_numa = mem_numa_nodes.get(domain_idx).copied().unwrap_or(0);
        let address_ptrs = if args.csv { bench.address_ptrs(domain_idx) } else { Vec::new() };

        // Only surface per-domain placement on real NUMA hardware (>1 node);
        // on a single-domain host we stay quiet, matching legacy behavior.
        if crate::numa::feature_enabled() && crate::numa::num_nodes() > 1 {
            if num_domains > 1 {
                eprintln!();
            }
            eprintln!("    {} {}", scolor.paint("Memory domain:"), fmt_mem_numa(mem_numa));
            // Default first-touch can split the shared cache lines across nodes;
            // when that happens the per-pair latency mixes local and remote
            // accesses and the numbers are not meaningful. (Bound placement is
            // verified single-node, so this only fires for default placement.)
            if mem_numa_spanned.get(domain_idx).copied().unwrap_or(false) {
                eprintln!(
                    "    {}",
                    Color::Yellow.bold().paint(
                        "WARN: this domain's shared buffer landed on more than one NUMA \
                         node (default first-touch). Latencies mix local and remote \
                         access; pass --numa <node> to pin placement."
                    )
                );
            }
        }

        // Print the column header for this domain's matrix.
        eprint!("    {: >3}", "");
        for j in cores {
            eprint!(" {: >4}{: >4}", j.id, "");
        }
        eprintln!();

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
                let ping_numa = core_numa.get(&core_i.id).copied().unwrap_or(0);
                let pong_numa = core_numa.get(&core_j.id).copied().unwrap_or(0);

                // We add 1 warmup cycle first
                let durations = bench.run(domain_idx, (core_i, core_j), clock, num_iterations, 1+num_samples);

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
                        let collapsed = sample_agg.collapse(&stats);
                        pair_collapsed.push(collapsed);

                        cell_records.push(CellRecord {
                            ping_core: core_i.id,
                            pong_core: core_j.id,
                            ping_numa,
                            pong_numa,
                            mem_numa,
                            collapsed,
                        });

                        if args.csv {
                            let vaddr = address_ptrs.get(addr_idx).copied().unwrap_or(0);
                            per_address_rows.push(PerAddressRow {
                                ping_core: core_i.id,
                                pong_core: core_j.id,
                                ping_numa,
                                pong_numa,
                                mem_numa,
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

                // Display matrix entry
                let all_samples_arr = ndarray::arr1(&all_samples);
                let mean = format!("{: >4.0}", all_samples_arr.mean().unwrap());
                let stddev = format!("+-{: <2.0}", all_samples_arr.std(1.0).min(99.0) / (num_samples as f64).sqrt());
                eprint!(" {}{}", mcolor.paint(mean), scolor.paint(stddev));
                let _ = std::io::stderr().lock().flush();
            }
            eprintln!();
        }
    }

    eprintln!();

    // Per-pair summaries, flat across all domains and addresses. Used for both
    // the console min/max stats and per_pair.csv.
    let pair_groups = group_cells(&cell_records, |r| (r.ping_core, r.pong_core));

    // Track min/max of the per-pair mean and median across all pairs.
    let mut min_latency_by_mean: Option<(f64, usize, usize)> = None;
    let mut max_latency_by_mean: Option<(f64, usize, usize)> = None;
    let mut min_latency_by_median: Option<(f64, usize, usize)> = None;
    let mut max_latency_by_median: Option<(f64, usize, usize)> = None;

    for (rep, stats) in &pair_groups {
        let r = &cell_records[*rep];
        let (pi, pj) = (r.ping_core, r.pong_core);

        match &min_latency_by_mean {
            None => min_latency_by_mean = Some((stats.mean, pi, pj)),
            Some((v, _, _)) if stats.mean < *v => min_latency_by_mean = Some((stats.mean, pi, pj)),
            _ => {}
        }
        match &max_latency_by_mean {
            None => max_latency_by_mean = Some((stats.mean, pi, pj)),
            Some((v, _, _)) if stats.mean > *v => max_latency_by_mean = Some((stats.mean, pi, pj)),
            _ => {}
        }
        match &min_latency_by_median {
            None => min_latency_by_median = Some((stats.median, pi, pj)),
            Some((v, _, _)) if stats.median < *v => min_latency_by_median = Some((stats.median, pi, pj)),
            _ => {}
        }
        match &max_latency_by_median {
            None => max_latency_by_median = Some((stats.median, pi, pj)),
            Some((v, _, _)) if stats.median > *v => max_latency_by_median = Some((stats.median, pi, pj)),
            _ => {}
        }

        if args.csv {
            // Feed the legacy matrix the per-pair mean of the collapsed values.
            if let (Some(&ci), Some(&cj)) = (id_to_idx.get(&pi), id_to_idx.get(&pj)) {
                legacy_matrix[ci][cj] = stats.mean;
                if bench.is_symmetric() {
                    legacy_matrix[cj][ci] = stats.mean;
                }
            }
        }
    }

    // Print min/max latency (using median-based, which is more robust)
    eprintln!("    {} (robust to outliers):", scolor.paint("Median-based stats"));
    if let Some((min_val, pi, pj)) = min_latency_by_median {
        eprintln!("      Min  latency: {}ns cores: ({},{})",
                 mcolor.paint(format!("{:.1}", min_val)), pi, pj);
    }
    if let Some((max_val, pi, pj)) = max_latency_by_median {
        eprintln!("      Max  latency: {}ns cores: ({},{})",
                 mcolor.paint(format!("{:.1}", max_val)), pi, pj);
    }

    eprintln!("    {} (sensitive to outliers):", scolor.paint("Mean-based stats"));
    if let Some((min_val, pi, pj)) = min_latency_by_mean {
        eprintln!("      Min  latency: {}ns cores: ({},{})",
                 mcolor.paint(format!("{:.1}", min_val)), pi, pj);
    }
    if let Some((max_val, pi, pj)) = max_latency_by_mean {
        eprintln!("      Max  latency: {}ns cores: ({},{})",
                 mcolor.paint(format!("{:.1}", max_val)), pi, pj);
    }

    // Print overall mean latency (computed from running totals)
    if running_count > 0 {
        let overall_mean = running_sum / running_count as f64;
        eprintln!("    Overall mean latency: {}ns", mcolor.paint(format!("{:.1}", overall_mean)));
    }

    // Write CSV files if requested
    if args.csv {
        // per_domain.csv now emitted even for a single domain; group across mem_numa
        let domain_groups = group_cells(&cell_records, |r| (r.ping_core, r.pong_core, r.mem_numa));
        write_csv_files(
            &args.csv_output_prefix,
            bench_name,
            &per_address_rows,
            &cell_records,
            &domain_groups,
            &pair_groups,
            num_domains > 0,
        );

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

fn write_csv_files(
    prefix: &str,
    bench_name: &str,
    per_address_rows: &[PerAddressRow],
    records: &[CellRecord],
    domain_groups: &[(usize, Stats)],
    pair_groups: &[(usize, Stats)],
    emit_domain: bool,
) {
    // Write per-address CSV (one row per cell; five stats over raw samples)
    if let Some(mut writer) = csv_output::create_csv(prefix, bench_name, "per_address") {
        writeln!(writer, "ping_core,pong_core,ping_numa,pong_numa,mem_numa,address,vaddr,mean_latency,median_latency,min_latency,max_latency,cv_percent").unwrap();
        for row in per_address_rows {
            writeln!(writer, "{},{},{},{},{},{},0x{:x},{:.2},{:.2},{:.2},{:.2},{:.2}",
                    row.ping_core, row.pong_core,
                    row.ping_numa, row.pong_numa, row.mem_numa,
                    row.address, row.vaddr,
                    row.mean, row.median, row.min, row.max, row.cv_percent).unwrap();
        }
    }

    // Write per-domain CSV (one row per (pair, mem_numa); only when >1 domain).
    if emit_domain {
        if let Some(mut writer) = csv_output::create_csv(prefix, bench_name, "per_domain") {
            writeln!(writer, "ping_core,pong_core,ping_numa,pong_numa,mem_numa,mean_latency,median_latency,min_latency,max_latency,cv_percent").unwrap();
            for (rep, stats) in domain_groups {
                let r = &records[*rep];
                writeln!(writer, "{},{},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2}",
                        r.ping_core, r.pong_core,
                        r.ping_numa, r.pong_numa, r.mem_numa,
                        stats.mean, stats.median, stats.min, stats.max, stats.cv_percent).unwrap();
            }
        }
    }

    // Write per-pair CSV (five stats over the sample-agg-collapsed values of the
    // pair, flat across all domains)
    if let Some(mut writer) = csv_output::create_csv(prefix, bench_name, "per_pair") {
        writeln!(writer, "ping_core,pong_core,ping_numa,pong_numa,mean_latency,median_latency,min_latency,max_latency,cv_percent").unwrap();
        for (rep, stats) in pair_groups {
            let r = &records[*rep];
            writeln!(writer, "{},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2}",
                    r.ping_core, r.pong_core,
                    r.ping_numa, r.pong_numa,
                    stats.mean, stats.median, stats.min, stats.max, stats.cv_percent).unwrap();
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

    // --- Generic per-cell group-by (the NUMA-aware aggregation) -------------

    fn rec(ping: usize, pong: usize, mem: i32, collapsed: f64) -> CellRecord {
        CellRecord {
            ping_core: ping,
            pong_core: pong,
            ping_numa: 0,
            pong_numa: 0,
            mem_numa: mem,
            collapsed,
        }
    }

    /// Assert a group's Stats equal `Stats::compute` over `expected`.
    fn group_matches(group: &(usize, Stats), expected: &[f64]) {
        let exp = Stats::compute(expected).unwrap();
        approx(group.1.mean, exp.mean);
        approx(group.1.median, exp.median);
        approx(group.1.min, exp.min);
        approx(group.1.max, exp.max);
        approx(group.1.cv_percent, exp.cv_percent);
    }

    #[test]
    fn group_by_domain_and_pair() {
        // 2 pairs x 2 domains, mixed address counts (so groups have different
        // sizes and the two modes are non-trivial).
        //   pair (1,0): domain 0 -> [10, 20], domain 1 -> [30, 50]
        //   pair (2,0): domain 0 -> [5],      domain 1 -> [15, 25]
        let records = vec![
            rec(1, 0, 0, 10.0),
            rec(1, 0, 0, 20.0),
            rec(1, 0, 1, 30.0),
            rec(1, 0, 1, 50.0),
            rec(2, 0, 0, 5.0),
            rec(2, 0, 1, 15.0),
            rec(2, 0, 1, 25.0),
        ];

        // per_domain: group by (ping, pong, mem_numa), first-seen order.
        let domain = group_cells(&records, |r| (r.ping_core, r.pong_core, r.mem_numa));
        assert_eq!(domain.len(), 4);
        group_matches(&domain[0], &[10.0, 20.0]);
        group_matches(&domain[1], &[30.0, 50.0]);
        group_matches(&domain[2], &[5.0]); // single-element group
        group_matches(&domain[3], &[15.0, 25.0]);
        // The single-element domain group is degenerate.
        approx(domain[2].1.cv_percent, 0.0);
        approx(domain[2].1.min, domain[2].1.max);
        // Representative record carries the group's constant key columns.
        let r0 = &records[domain[0].0];
        assert_eq!((r0.ping_core, r0.pong_core, r0.mem_numa), (1, 0, 0));
        let r2 = &records[domain[2].0];
        assert_eq!((r2.ping_core, r2.pong_core, r2.mem_numa), (2, 0, 0));

        // per_pair: flat across domains, group by (ping, pong).
        let pair = group_cells(&records, |r| (r.ping_core, r.pong_core));
        assert_eq!(pair.len(), 2);
        group_matches(&pair[0], &[10.0, 20.0, 30.0, 50.0]);
        group_matches(&pair[1], &[5.0, 15.0, 25.0]);
        let p0 = &records[pair[0].0];
        assert_eq!((p0.ping_core, p0.pong_core), (1, 0));
    }

    #[test]
    fn group_by_single_domain_pair_is_degenerate() {
        // One pair, one domain, one address -> single collapsed value.
        let records = vec![rec(3, 1, 0, 42.0)];
        let pair = group_cells(&records, |r| (r.ping_core, r.pong_core));
        assert_eq!(pair.len(), 1);
        approx(pair[0].1.cv_percent, 0.0);
        approx(pair[0].1.min, 42.0);
        approx(pair[0].1.max, 42.0);
        approx(pair[0].1.mean, 42.0);
        approx(pair[0].1.median, 42.0);
    }
}
