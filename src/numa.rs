//! NUMA-aware shared-memory placement.
//!
//! Two compiled variants, selected by `cfg(all(feature = "numa", target_os =
//! "linux"))`:
//!
//! * Feature ON (Linux): a real backend built on the kernel NUMA syscalls
//!   (`mbind` / `move_pages` via the `libc` crate) and sysfs topology discovery
//!   (`/sys/devices/system/node/...`).
//! * Feature OFF (or non-Linux): a thin stub that reports a single default
//!   domain and allocates from the global heap.
//!
//! The real backend uses no system NUMA library: discovery reads sysfs and
//! placement is done with raw `mmap` + `mbind` + `move_pages` syscalls. This
//! drops the build-time `libnuma-dev` dependency the previous libnuma FFI
//! required.
//!
//! Resolved memory placement is reported as an `i32` node id, where `-1` means
//! "unverified": for kernel-default placement the host may not be able to
//! report where the pages landed (e.g. `move_pages` unsupported), and that is
//! not a fatal condition.

/// Where the shared cache lines for one benchmark domain should live.
///
/// Defined unconditionally (used by `parse_numa` and its unit tests, which run
/// with the feature off) so it does not depend on the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemPlacement {
    /// Kernel-default placement (current behavior; no binding).
    Default,
    /// Bind to a specific NUMA node id.
    Bound(usize),
}

/// True iff this binary was built with the `numa` feature.
///
/// Kept as a tiny pure helper so the "feature off rejects --numa" branch in
/// `parse_numa` is unit-testable without toggling features at test time.
pub const fn feature_enabled() -> bool {
    cfg!(all(feature = "numa", target_os = "linux"))
}

// ===========================================================================
// Shared NUMA topology discovery (works regardless of feature flag).
// ===========================================================================

const NODE_ROOT: &str = "/sys/devices/system/node";

/// Parse a Linux "cpulist"/"nodelist" string like "0", "0-3", "0,2-3" into a
/// sorted, deduped `Vec<usize>`. Unparseable fragments are skipped.
fn parse_list(s: &str) -> Vec<usize> {
    let mut v = Vec::new();
    for part in s.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                for id in a..=b {
                    v.push(id);
                }
            }
        } else if let Ok(id) = part.parse::<usize>() {
            v.push(id);
        }
    }
    v.sort_unstable();
    v.dedup();
    v
}

/// The set of online NUMA nodes, read once from
/// `/sys/devices/system/node/online`. Empty when sysfs is absent.
fn online_nodes() -> &'static [usize] {
    use std::sync::OnceLock;
    static ONLINE: OnceLock<Vec<usize>> = OnceLock::new();
    ONLINE.get_or_init(|| {
        std::fs::read_to_string(format!("{NODE_ROOT}/online"))
            .map(|s| parse_list(&s))
            .unwrap_or_default()
    })
}

/// Number of NUMA nodes: `max(online id) + 1` so that `parse_numa`'s
/// `id < num_nodes` validation and the `0..num_nodes` discovery loop both
/// cover every online node, even if node ids are non-contiguous.
/// (`node_is_online` guards the non-contiguous gaps.)
/// Works regardless of feature flag by reading sysfs.
pub fn num_nodes() -> usize {
    num_nodes_from(online_nodes())
}

/// Pure core of [`num_nodes`]: `max(id) + 1`, or 1 when the set is empty.
/// Split out so the non-contiguous-id and empty (sysfs-absent) cases are
/// unit-testable without real sysfs.
fn num_nodes_from(online: &[usize]) -> usize {
    online.iter().copied().max().map_or(1, |m| m + 1)
}

// ===========================================================================
// Feature ON, Linux: real kernel-syscall backend.
// ===========================================================================
#[cfg(all(feature = "numa", target_os = "linux"))]
mod imp {
    use super::MemPlacement;
    use std::os::raw::{c_int, c_void};
    use std::sync::OnceLock;

    const CACHELINE_SIZE: usize = 64;

    /// System page size, queried once.
    fn page_size() -> usize {
        // SAFETY: sysconf is always safe to call with a valid name.
        let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if v <= 0 {
            4096
        } else {
            v as usize
        }
    }

    // ----- sysfs topology discovery (no system NUMA library) ----------------
    // Note: parse_list and online_nodes are now shared at module level

    /// True iff the host exposes the NUMA sysfs hierarchy with >= 1 online node.
    /// This is the syscall-backend replacement for libnuma's `numa_available()`.
    fn numa_available_sysfs() -> bool {
        std::path::Path::new(super::NODE_ROOT).is_dir() && !super::online_nodes().is_empty()
    }

    /// Real cpu -> node map, built once by scanning each online node's cpulist.
    fn cpu_node_map() -> &'static std::collections::HashMap<usize, usize> {
        static MAP: OnceLock<std::collections::HashMap<usize, usize>> = OnceLock::new();
        MAP.get_or_init(|| {
            let mut map = std::collections::HashMap::new();
            for &node in super::online_nodes() {
                if let Ok(s) =
                    std::fs::read_to_string(format!("{}/node{}/cpulist", super::NODE_ROOT, node))
                {
                    for cpu in super::parse_list(&s) {
                        map.insert(cpu, node);
                    }
                }
            }
            map
        })
    }

    /// Startup hook. Intentionally a no-op: a binary built with the `numa`
    /// feature must run cleanly on a host with no NUMA topology (we
    /// cross-compile and ship a single binary), so there is no fatal startup
    /// guard. Detection is lazy: `num_nodes()` returns 1 and `node_of_cpu()`
    /// returns 0 on a host without a usable NUMA hierarchy, and a no-`--numa`
    /// run uses default placement. Kept for API symmetry with the stub.
    pub fn init() {}

    /// Whether NUMA is usable. Part of the documented API surface; not
    /// currently called from the benchmark driver.
    #[allow(dead_code)]
    pub fn available() -> bool {
        numa_available_sysfs()
    }

    /// Whether `node` is in the online set.
    fn node_is_online(node: usize) -> bool {
        super::online_nodes().contains(&node)
    }

    pub fn node_of_cpu(cpu: usize) -> usize {
        // sysfs cpu->node map; cpus not found (offline/unknown) clamp to 0.
        cpu_node_map().get(&cpu).copied().unwrap_or(0)
    }

    /// Per-node topology for the console summary.
    pub struct NodeInfo {
        pub node: usize,
        pub cpus: Vec<usize>,
        pub mem_bytes: u64,
    }

    pub struct Topology {
        pub nodes: Vec<NodeInfo>,
    }

    pub fn discover() -> Topology {
        // Only report the online nodes (skips non-contiguous gaps).
        let nodes = super::online_nodes().iter().map(|&node| real_node_info(node)).collect();
        Topology { nodes }
    }

    fn real_node_info(node: usize) -> NodeInfo {
        // CPU list from sysfs.
        let cpus = std::fs::read_to_string(format!("{}/node{}/cpulist", super::NODE_ROOT, node))
            .map(|s| super::parse_list(&s))
            .unwrap_or_default();

        // Total memory from the node's meminfo line:
        //   "Node <node> MemTotal:    <kB> kB"
        let mem_bytes = std::fs::read_to_string(format!("{}/node{}/meminfo", super::NODE_ROOT, node))
            .ok()
            .and_then(|s| {
                s.lines().find_map(|line| {
                    if line.contains("MemTotal:") {
                        line.split_whitespace()
                            .rev()
                            .nth(1) // value is the token before the trailing "kB"
                            .and_then(|kb| kb.parse::<u64>().ok())
                            .map(|kb| kb * 1024)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(0);

        NodeInfo {
            node,
            cpus,
            mem_bytes,
        }
    }

    // ----- raw NUMA syscall wrappers (no system NUMA library) ---------------

    /// `mbind(2)` via `libc::syscall`. Sets the memory policy `mode` (with the
    /// given nodemask) for the address range `[addr, addr+len)`.
    ///
    /// SAFETY: the caller must ensure `[addr, addr+len)` is a valid mapping it
    /// owns, that `nodemask` points to at least `ceil(maxnode / bits-per-ulong)`
    /// readable `c_ulong` words, and that `mode`/`flags` are valid mbind args.
    unsafe fn mbind(
        addr: *mut c_void,
        len: usize,
        mode: c_int,
        nodemask: *const libc::c_ulong,
        maxnode: libc::c_ulong,
        flags: c_int,
    ) -> i64 {
        libc::syscall(
            libc::SYS_mbind,
            addr,
            len,
            mode,
            nodemask,
            maxnode,
            flags,
        ) as i64
    }

    /// `move_pages(2)` via `libc::syscall`. With `nodes == NULL` this is query
    /// mode: each `status[i]` is filled with the node id of page `pages[i]`.
    ///
    /// SAFETY: the caller must ensure `pages` points to `count` valid page
    /// pointers, `status` points to `count` writable `c_int` slots, and `nodes`
    /// is either NULL (query) or points to `count` valid node ids.
    unsafe fn move_pages(
        pid: c_int,
        count: libc::c_ulong,
        pages: *mut *mut c_void,
        nodes: *const c_int,
        status: *mut c_int,
        flags: c_int,
    ) -> i64 {
        libc::syscall(
            libc::SYS_move_pages,
            pid,
            count,
            pages,
            nodes,
            status,
            flags,
        ) as i64
    }

    /// A shared-memory buffer placed on a NUMA node.
    ///
    /// The region is page-rounded, zero-initialized (touched), and 64-byte
    /// aligned (mmap returns page-aligned memory), so it can be reinterpreted as
    /// the benches' atomic / `repr(C)` cache-line layouts.
    pub struct NumaBuffer {
        ptr: *mut u8,
        /// Allocation length in bytes (page-rounded).
        len: usize,
        /// Resolved node the pages landed on, or `-1` if placement could not be
        /// verified (default placement on a host where `move_pages` cannot
        /// report).
        node: i32,
        /// True when the buffer's pages landed on more than one NUMA node. Only
        /// possible under `Default` placement (Bound is verified single-node);
        /// surfaced so the driver can warn that default first-touch split the
        /// shared cache lines across nodes.
        spanned: bool,
    }

    // The buffer is only ever shared as `&[Atomic*]` across threads, which is
    // Sync. The raw pointer itself does not auto-impl Send/Sync; we assert it
    // is safe to move the owner across threads (we never do, but run_bench holds
    // the bench by shared ref).
    unsafe impl Send for NumaBuffer {}
    unsafe impl Sync for NumaBuffer {}

    impl NumaBuffer {
        /// Allocate `n_cachelines` 64-byte cache lines on `placement`.
        ///
        /// `Bound(n)` is verified: every page must land on node `n` (via
        /// `move_pages`) or this is a hard error. `Default` is best-effort: the
        /// resolved node is reported when the kernel can report it, otherwise
        /// `node = -1` (unverified) and the allocation still succeeds.
        pub fn alloc(placement: MemPlacement, n_cachelines: usize) -> Result<NumaBuffer, String> {
            assert!(n_cachelines >= 1, "n_cachelines must be >= 1");
            let want = n_cachelines * CACHELINE_SIZE;
            Self::alloc_real(placement, want)
        }

        fn alloc_real(placement: MemPlacement, want: usize) -> Result<NumaBuffer, String> {
            // Validate the requested node against the online set up front (node
            // ids may be non-contiguous, so `< num_nodes` is not sufficient).
            if let MemPlacement::Bound(node) = placement {
                if !node_is_online(node) {
                    return Err(format!(
                        "NUMA node {node} is not online on this host"
                    ));
                }
            }

            let psize = page_size();
            // Page-round so move_pages reports one status per whole page and the
            // allocation owns complete pages.
            let len = want.div_ceil(psize) * psize;

            // Page-aligned, zero-filled anonymous mapping (MAP_ANONYMOUS zeroes
            // the pages, preserving the all-zero atomic-init invariant).
            // SAFETY: addr=NULL lets the kernel choose; len > 0; standard
            // private anonymous flags; fd=-1, offset=0 as required for
            // MAP_ANONYMOUS. Returns MAP_FAILED on error.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(format!(
                    "mmap of {len} bytes failed (placement {placement:?})"
                ));
            }
            let ptr = ptr as *mut u8;
            // mmap returns page-aligned memory and pages are a multiple of 64,
            // so the region is always cache-line aligned. Assert the intent the
            // atomic-layout reinterpretation in `as_slice` relies on.
            debug_assert_eq!(
                ptr as usize % CACHELINE_SIZE,
                0,
                "mmap region not {CACHELINE_SIZE}-byte aligned"
            );

            // For Bound(n), set an MPOL_BIND policy on the region before
            // faulting it in so first-touch lands the pages on `n`. Default uses
            // no policy (kernel default / first-touch).
            if let MemPlacement::Bound(node) = placement {
                // Nodemask with bit `node` set, allocated in whole c_ulong
                // words. `maxnode` is the number of bits in the mask (the
                // kernel's convention, as used by libnuma): we pass the full bit
                // capacity of the allocated words. Note the kernel rejects
                // maxnode==1 (EINVAL), so always allocate at least one full word
                // and report its whole bit width.
                let word_bits = std::mem::size_of::<libc::c_ulong>() * 8;
                let words = (node / word_bits) + 1;
                let mut nodemask = vec![0 as libc::c_ulong; words];
                // Explicit c_ulong so the shifted bit cannot narrow on exotic
                // targets where the literal would default to a smaller type.
                nodemask[node / word_bits] |= (1 as libc::c_ulong) << (node % word_bits);
                let maxnode = (words * word_bits) as libc::c_ulong;
                // SAFETY: ptr..ptr+len is our valid mapping; nodemask points to
                // `words` c_ulong words = `maxnode` bits; maxnode matches.
                let rc = unsafe {
                    mbind(
                        ptr as *mut c_void,
                        len,
                        libc::MPOL_BIND,
                        nodemask.as_ptr(),
                        maxnode,
                        0,
                    )
                };
                // The glibc syscall wrapper returns -1 on error (errno set) and
                // 0 on success; compare to -1 so an unexpected positive return
                // is not misread as a failure.
                if rc == -1 {
                    // Capture errno before munmap, which may clobber it.
                    let err = std::io::Error::last_os_error();
                    // SAFETY: ptr/len came from mmap above; unmap exactly once.
                    unsafe { libc::munmap(ptr as *mut c_void, len) };
                    return Err(format!(
                        "mbind(MPOL_BIND, node {node}) failed: {err}"
                    ));
                }
            }

            // First-touch: write zero to every page so pages are faulted in and
            // physically placed (under the policy set above) before we query.
            // SAFETY: ptr..ptr+len is a valid, owned region of `len` bytes.
            unsafe {
                std::ptr::write_bytes(ptr, 0u8, len);
            }

            // Verify/resolve placement with move_pages in query mode
            // (nodes=NULL): it fills `status[i]` with the node id of page i.
            let n_pages = len / psize;
            let mut pages: Vec<*mut c_void> = (0..n_pages)
                .map(|i| unsafe { ptr.add(i * psize) as *mut c_void })
                .collect();
            let mut status: Vec<c_int> = vec![-1; n_pages];
            // SAFETY: pid 0 = current process; pages[] are valid page-aligned
            // addresses within our allocation; nodes=NULL selects query mode;
            // status has n_pages slots; flags 0.
            let rc = unsafe {
                move_pages(
                    0,
                    n_pages as libc::c_ulong,
                    pages.as_mut_ptr(),
                    std::ptr::null(),
                    status.as_mut_ptr(),
                    0,
                )
            };

            // Per man 2 move_pages, status[] is only valid if the call returned
            // 0. For Bound placement a failed/unverifiable query is fatal; for
            // Default it is non-fatal and we report node = -1 (unverified).
            // Capture errno immediately after the syscall, before any munmap
            // (which would clobber it) on the error paths below. The glibc
            // wrapper returns -1 on error; compare to -1 for clarity.
            let query_err = std::io::Error::last_os_error();
            let (resolved, spanned): (i32, bool) = if rc == -1 {
                match placement {
                    MemPlacement::Bound(node) => {
                        let err = query_err;
                        // SAFETY: ptr/len came from mmap above; unmap exactly once.
                        unsafe { libc::munmap(ptr as *mut c_void, len) };
                        return Err(format!(
                            "move_pages query failed: {err}; cannot verify \
                             placement on node {node}"
                        ));
                    }
                    MemPlacement::Default => (-1, false),
                }
            } else {
                match placement {
                    MemPlacement::Bound(node) => {
                        // Every page must be on `node`.
                        let bad = status
                            .iter()
                            .copied()
                            .any(|s| s < 0 || s as usize != node);
                        if bad {
                            // SAFETY: ptr/len came from mmap above; unmap once.
                            unsafe { libc::munmap(ptr as *mut c_void, len) };
                            return Err(format!(
                                "buffer requested on node {node} but did not land \
                                 there (move_pages status={status:?})"
                            ));
                        }
                        (node as i32, false)
                    }
                    MemPlacement::Default => {
                        // Best-effort: report the first valid page's node (else
                        // -1, unverified) and flag whether the pages landed on
                        // more than one distinct node (first-touch split).
                        let first = status.iter().copied().find(|&s| s >= 0).unwrap_or(-1);
                        let spanned = first >= 0
                            && status.iter().copied().any(|s| s >= 0 && s != first);
                        (first, spanned)
                    }
                }
            };

            Ok(NumaBuffer {
                ptr,
                len,
                node: resolved,
                spanned,
            })
        }

        /// Resolved NUMA node the buffer's pages are on, or `-1` if unverified.
        pub fn node(&self) -> i32 {
            self.node
        }

        /// True when the buffer's pages landed on more than one NUMA node
        /// (only possible under `Default` first-touch placement).
        pub fn spanned(&self) -> bool {
            self.spanned
        }

        /// Reinterpret the zeroed, 64-byte-aligned region as `&[T]`.
        ///
        /// Sound only for `T` that: (a) accept an all-zero bit pattern as a
        /// valid value (atomics and `repr(C)` cache-line structs of atomics do),
        /// and (b) have `align_of::<T>() <= 64`. We assert size/align/coverage.
        pub fn as_slice<T>(&self, count: usize) -> &[T] {
            let tsize = std::mem::size_of::<T>();
            let talign = std::mem::align_of::<T>();
            assert!(talign <= CACHELINE_SIZE, "T over-aligned for NumaBuffer");
            assert_eq!(
                self.ptr as usize % talign,
                0,
                "NumaBuffer not aligned for T"
            );
            assert!(
                tsize.checked_mul(count).map_or(false, |need| need <= self.len),
                "NumaBuffer too small: need {}*{} bytes, have {}",
                tsize,
                count,
                self.len
            );
            // SAFETY: ptr is non-null, aligned for T (asserted), points to
            // `count*size_of::<T>()` bytes (asserted) that are zero-initialized,
            // and T accepts the all-zero pattern. The slice borrows &self so it
            // cannot outlive the buffer.
            unsafe { std::slice::from_raw_parts(self.ptr as *const T, count) }
        }
    }

    impl Drop for NumaBuffer {
        fn drop(&mut self) {
            // SAFETY: ptr/len came from mmap; munmap exactly once.
            unsafe { libc::munmap(self.ptr as *mut c_void, self.len) };
        }
    }


}

// ===========================================================================
// Feature OFF or non-Linux: heap-backed single-domain stub.
// ===========================================================================
// This stub mirrors the full feature-on API surface so callers compile
// unchanged; some of it (e.g. `available`, the topology fields) is unused in
// the single-domain default build.
#[cfg(not(all(feature = "numa", target_os = "linux")))]
#[allow(dead_code)]
mod imp {
    use super::MemPlacement;

    const CACHELINE_SIZE: usize = 64;

    pub fn init() {}

    pub fn available() -> bool {
        false
    }

    pub fn node_of_cpu(_cpu: usize) -> usize {
        0
    }

    pub struct NodeInfo {
        pub node: usize,
        pub cpus: Vec<usize>,
        pub mem_bytes: u64,
    }

    pub struct Topology {
        pub nodes: Vec<NodeInfo>,
    }

    pub fn discover() -> Topology {
        Topology { nodes: Vec::new() }
    }


    /// Heap-backed buffer reporting node 0.
    pub struct NumaBuffer {
        ptr: *mut u8,
        len: usize,
        layout: std::alloc::Layout,
    }

    unsafe impl Send for NumaBuffer {}
    unsafe impl Sync for NumaBuffer {}

    impl NumaBuffer {
        pub fn alloc(placement: MemPlacement, n_cachelines: usize) -> Result<NumaBuffer, String> {
            // The feature-off path should only ever see Default (parse_numa
            // rejects explicit --numa when the feature is off), but handle Bound
            // defensively.
            if let MemPlacement::Bound(n) = placement {
                return Err(format!(
                    "NUMA placement on node {n} requires the `numa` build feature"
                ));
            }
            assert!(n_cachelines >= 1, "n_cachelines must be >= 1");
            let len = n_cachelines * CACHELINE_SIZE;
            let layout = std::alloc::Layout::from_size_align(len, CACHELINE_SIZE)
                .map_err(|e| format!("bad layout: {e}"))?;
            // SAFETY: layout has non-zero size and valid alignment.
            let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
            if ptr.is_null() {
                return Err("heap allocation failed".to_string());
            }
            Ok(NumaBuffer { ptr, len, layout })
        }

        /// Resolved node is always 0 for the heap-backed stub.
        pub fn node(&self) -> i32 {
            0
        }

        /// The heap-backed stub is single-domain; pages never span nodes.
        pub fn spanned(&self) -> bool {
            false
        }

        pub fn as_slice<T>(&self, count: usize) -> &[T] {
            let tsize = std::mem::size_of::<T>();
            let talign = std::mem::align_of::<T>();
            assert!(talign <= CACHELINE_SIZE, "T over-aligned for NumaBuffer");
            assert_eq!(self.ptr as usize % talign, 0, "NumaBuffer not aligned for T");
            assert!(
                tsize.checked_mul(count).map_or(false, |need| need <= self.len),
                "NumaBuffer too small: need {}*{} bytes, have {}",
                tsize,
                count,
                self.len
            );
            // SAFETY: same invariants as the feature-on path: aligned, zeroed,
            // sized, T accepts all-zero, slice borrows &self.
            unsafe { std::slice::from_raw_parts(self.ptr as *const T, count) }
        }
    }

    impl Drop for NumaBuffer {
        fn drop(&mut self) {
            // SAFETY: ptr came from alloc_zeroed with this exact layout.
            unsafe { std::alloc::dealloc(self.ptr, self.layout) };
        }
    }
}

#[allow(unused_imports)]
pub use imp::{
    available, discover, init, node_of_cpu, NodeInfo, NumaBuffer,
    Topology,
};

/// Summary data for main to format as user-facing text. The numa module
/// remains a pure detection/placement API; main owns the wording/printing.
pub struct Summary {
    pub feature_enabled: bool,
    pub num_nodes: usize,
    pub topology: Option<Topology>,
}

/// Gather summary information about NUMA for printing by the caller.
pub fn summary_info() -> Summary {
    let feature = feature_enabled();
    let nodes = num_nodes();
    let topo = if feature { Some(discover()) } else { None };
    Summary {
        feature_enabled: feature,
        num_nodes: nodes,
        topology: topo,
    }
}

#[cfg(test)]
mod tests {
    use super::{num_nodes_from, parse_list};

    #[test]
    fn parse_list_basic() {
        assert_eq!(parse_list("0"), vec![0]);
        assert_eq!(parse_list("0-3"), vec![0, 1, 2, 3]);
        assert_eq!(parse_list("0,2-3"), vec![0, 2, 3]);
        // Sorted and deduped.
        assert_eq!(parse_list("3,1,1,2"), vec![1, 2, 3]);
    }

    #[test]
    fn parse_list_skips_malformed_tokens() {
        // Empty string and whitespace -> no ids.
        assert_eq!(parse_list(""), Vec::<usize>::new());
        assert_eq!(parse_list("  "), Vec::<usize>::new());
        // A bad token is skipped but good neighbours are kept.
        assert_eq!(parse_list("0,x,2"), vec![0, 2]);
        assert_eq!(parse_list("1,,2"), vec![1, 2]);
        // Half-open / non-numeric range bounds are dropped whole.
        assert_eq!(parse_list("1-"), Vec::<usize>::new());
        assert_eq!(parse_list("-3"), Vec::<usize>::new());
        assert_eq!(parse_list("a-b"), Vec::<usize>::new());
        // A reversed range yields nothing (a..=b is empty when a > b).
        assert_eq!(parse_list("2-1"), Vec::<usize>::new());
        // Mixed good/bad fragments.
        assert_eq!(parse_list("0-2,x,5"), vec![0, 1, 2, 5]);
    }

    #[test]
    fn num_nodes_from_empty_is_one() {
        // Simulates sysfs absent: online_nodes() returns an empty slice.
        assert_eq!(num_nodes_from(&[]), 1);
    }

    #[test]
    fn num_nodes_from_contiguous() {
        assert_eq!(num_nodes_from(&[0]), 1);
        assert_eq!(num_nodes_from(&[0, 1]), 2);
    }

    #[test]
    fn num_nodes_from_non_contiguous_uses_max_plus_one() {
        // Non-contiguous ids: count must cover the highest id so parse_numa's
        // `id < num_nodes` validation accepts every online node.
        assert_eq!(num_nodes_from(&[0, 3]), 4);
        assert_eq!(num_nodes_from(&[2, 5]), 6);
    }
}
