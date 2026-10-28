//! Host resource-pressure telemetry for supervised `pi` runs.
//!
//! Motivation (see ,
//!
//! ): the supervisor
//! already bounds *logical* budgets — event count, stdout bytes, tokens,
//! provider cost, wall/stall clocks — but it has been **blind to physical host
//! pressure**. The 2026-09-13 incident (an SHUTTLE process holding ~16.43 GiB RSS)
//! was invisible to telemetry and had no admission-control brake. This module
//! closes that gap: it samples the *observed* combined resident memory of the
//! `shuttle` supervisor plus its isolated pi process group, alongside a best-effort
//! host memory-pressure signal, and trips an admission-control kill when the
//! default or configured ceiling is crossed.
//!
//! Design constraints honoured here:
//! * **Daemonless.** No background thread, no port, no resident collector. The
//!   supervisor calls [`ResourceSampler::sample`] on the loop tick it already
//!   owns; sampling is a synchronous best-effort snapshot.
//! * **§7 never-fabricate.** Every metric is `Option`: an unavailable probe
//!   (tool missing, TCC-blocked `ps`, unparseable output) yields `None`, never a
//!   guessed zero. Downstream receipts render missing metrics as `null` and mark
//!   them in `missing_fields`.
//! * **Red line #11 (shuttle owns no land rights).** The only *action* this module
//!   can take is to kill **shuttle's own child process group** in self-defence when
//!   a local ceiling is exceeded. It never touches other processes,
//!   never reclaims host memory, never negotiates with an external scheduler.
//! * **Injectable.** All OS reads go through the [`ResourceProbe`] trait so the
//!   sampler and every parser are unit-testable with hermetic fixtures.
//!
//! Portability: the real probe ([`SystemProbe`]) reads macOS `ps` / `vm_stat` /
//! `sysctl hw.memsize` / `sysctl vm.swapusage` and, on Linux, `ps` /
//! `/proc/meminfo`. Anything it cannot read degrades to `None`.

use serde::Serialize;
use std::process::{Command, Stdio};
use std::time::Duration;

/// A supervised run may override this ceiling, or explicitly disable it with
/// `SHUTTLE_MAX_PROC_RSS_BYTES=0`. Eight GiB is high enough for normal compiler/tool
/// bursts while stopping well before the 16.43 GiB incident magnitude.
pub const DEFAULT_MAX_PROC_RSS_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// One immutable resource observation folded into `RunTelemetry`.
///
/// All fields are `Option`/count so a run on a locked-down host still produces a
/// well-formed (mostly-`null`) record rather than lying with zeroes.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RunResources {
    /// Highest observed combined RSS of the `shuttle` supervisor and its pi process
    /// group, in bytes. `None` when `ps` could never be read across the run.
    pub peak_managed_rss_bytes: Option<u64>,
    /// Highest observed host memory-utilisation fraction in `[0.0, 1.0]`
    /// (`1.0 - available/total`). `None` when neither `vm_stat`+`hw.memsize`
    /// (macOS) nor `/proc/meminfo` (Linux) could be read.
    pub peak_host_mem_used_frac: Option<f64>,
    /// Advisory classification of `peak_host_mem_used_frac`. `None` mirrors it.
    pub peak_host_mem_level: Option<HostMemLevel>,
    /// Highest observed swap-in-use, in bytes (macOS `vm.swapusage`). `None`
    /// when unreadable or on hosts without a swap probe.
    pub peak_host_swap_used_bytes: Option<u64>,
    /// Number of successful sampling ticks that contributed to the peaks above.
    /// Zero means sampling never landed a single reading (all-`None` peaks).
    pub samples: u32,
    /// Machine-readable admission-control verdict, if a ceiling was crossed.
    /// `None` means no local resource ceiling tripped.
    pub admission_control: Option<AdmissionControl>,
}

/// Advisory host memory-pressure band. Purely informational — it never gates a
/// run on its own; only the explicit ceilings in [`ResourceGuards`] can kill.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostMemLevel {
    Normal,
    Warn,
    Critical,
}

impl HostMemLevel {
    /// Classify a `[0,1]` utilisation fraction. Bands are advisory heuristics,
    /// deliberately conservative: warn ≥ 0.85, critical ≥ 0.95.
    pub fn classify(frac: f64) -> Self {
        if frac >= 0.95 {
            Self::Critical
        } else if frac >= 0.85 {
            Self::Warn
        } else {
            Self::Normal
        }
    }
}

/// Which explicit local ceiling tripped admission control.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AdmissionControl {
    /// Stable reason code, mirrored into `RunTelemetry.limit_reason`.
    pub reason: String,
    /// The observed value that crossed the ceiling (bytes or percent×100).
    pub observed: u64,
    /// The configured ceiling that was crossed.
    pub limit: u64,
}

/// Injectable OS read surface. Each method returns the *raw* tool output so the
/// parsing logic stays pure and hermetically testable. `None` = unavailable.
pub trait ResourceProbe: Send {
    /// `ps` snapshot, one `"<pid> <pgid> <rss_kib>"` tuple per line.
    fn ps_pid_pgid_rss(&self) -> Option<String>;
    /// macOS `vm_stat` output (page counts + page-size header).
    fn vm_stat(&self) -> Option<String>;
    /// Total physical memory in bytes (macOS `sysctl -n hw.memsize`).
    fn mem_total_bytes(&self) -> Option<u64>;
    /// macOS `sysctl -n vm.swapusage` line.
    fn swap_usage(&self) -> Option<String>;
    /// Linux `/proc/meminfo` contents.
    fn meminfo(&self) -> Option<String>;
}

/// The production probe. Resolves `ps` through [`shuttle_runtime_tools`] (honouring
/// `SHUTTLE_PS_BIN` and the vendored bundle) and reads the rest by bare program name
/// off `PATH`. Every read is wrapped so a missing/blocked tool degrades to
/// `None` rather than propagating an error.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProbe;

impl SystemProbe {
    fn run(bin: &str, args: &[&str]) -> Option<String> {
        let mut command = Command::new(bin);
        command.args(args).stdin(Stdio::null());
        // A diagnostic probe must never become a new resource leak. Bound its
        // lifetime and captured bytes just like other SHUTTLE-owned subprocesses.
        let out = shuttle_runtime_tools::output_bounded(
            &mut command,
            Duration::from_millis(250),
            4 * 1024 * 1024,
            64 * 1024,
        )
        .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }
}

impl ResourceProbe for SystemProbe {
    fn ps_pid_pgid_rss(&self) -> Option<String> {
        let ps = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Ps).ok()?;
        // `-A` (BSD/GNU) lists every process; `=` drops the header. RSS is KiB
        // on both macOS and Linux. PID lets us include the shuttle supervisor while
        // PGID includes pi and every descendant in its isolated group.
        Self::run(&ps.to_string_lossy(), &["-Ao", "pid=,pgid=,rss="])
    }

    fn vm_stat(&self) -> Option<String> {
        Self::run("vm_stat", &[])
    }

    fn mem_total_bytes(&self) -> Option<u64> {
        Self::run("sysctl", &["-n", "hw.memsize"])?
            .trim()
            .parse::<u64>()
            .ok()
    }

    fn swap_usage(&self) -> Option<String> {
        Self::run("sysctl", &["-n", "vm.swapusage"])
    }

    fn meminfo(&self) -> Option<String> {
        std::fs::read_to_string("/proc/meminfo").ok()
    }
}

/// Local resource ceilings. Managed RSS is bounded by default; explicit `0`
/// disables it. Host-wide pressure remains opt-in because it is advisory and
/// can be caused by unrelated processes.
#[derive(Debug, Clone, Copy)]
pub struct ResourceGuards {
    /// Kill the pi group when combined shuttle + pi-group RSS exceeds this many
    /// bytes. `SHUTTLE_MAX_PROC_RSS_BYTES`; unset uses 8 GiB and `0` disables.
    pub max_proc_rss_bytes: Option<u64>,
    /// Kill the pi group when host memory utilisation exceeds this percent
    /// (`0..=100`). `SHUTTLE_HOST_MEM_PRESSURE_ABORT_PCT`; unset/`0` disables.
    pub host_mem_pressure_abort_pct: Option<u64>,
}

impl ResourceGuards {
    pub fn from_env() -> Self {
        Self {
            max_proc_rss_bytes: env_u64_budget(
                "SHUTTLE_MAX_PROC_RSS_BYTES",
                DEFAULT_MAX_PROC_RSS_BYTES,
            ),
            host_mem_pressure_abort_pct: env_u64_positive("SHUTTLE_HOST_MEM_PRESSURE_ABORT_PCT")
                .map(|v| v.min(100)),
        }
    }
}

impl Default for ResourceGuards {
    fn default() -> Self {
        Self {
            max_proc_rss_bytes: Some(DEFAULT_MAX_PROC_RSS_BYTES),
            host_mem_pressure_abort_pct: None,
        }
    }
}

fn env_u64_budget(name: &str, default: u64) -> Option<u64> {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(value) => Some(value),
            Err(_) => Some(default),
        },
        Err(_) => Some(default),
    }
}

fn env_u64_positive(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
}

/// The cadence at which the supervisor should call [`ResourceSampler::sample`].
/// `SHUTTLE_RESOURCE_SAMPLE_MS` (default 1000); `0` disables sampling entirely.
pub fn sample_interval() -> Option<Duration> {
    let raw = std::env::var("SHUTTLE_RESOURCE_SAMPLE_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(1000);
    if raw == 0 {
        None
    } else {
        Some(Duration::from_millis(raw))
    }
}

/// Stateful peak-tracker driven by the supervisor loop. Holds the injected probe
/// and the managed SHUTTLE/child identities, folds each snapshot into running peaks,
/// and evaluates admission-control ceilings.
pub struct ResourceSampler<P: ResourceProbe> {
    probe: P,
    supervisor_pid: i32,
    child_pgid: i32,
    guards: ResourceGuards,
    peak_managed_rss_bytes: Option<u64>,
    peak_host_mem_used_frac: Option<f64>,
    peak_host_swap_used_bytes: Option<u64>,
    samples: u32,
    admission_control: Option<AdmissionControl>,
}

impl<P: ResourceProbe> ResourceSampler<P> {
    pub fn new(probe: P, supervisor_pid: i32, child_pgid: i32, guards: ResourceGuards) -> Self {
        Self {
            probe,
            supervisor_pid,
            child_pgid,
            guards,
            peak_managed_rss_bytes: None,
            peak_host_mem_used_frac: None,
            peak_host_swap_used_bytes: None,
            samples: 0,
            admission_control: None,
        }
    }

    /// Take one best-effort snapshot and fold it into the running peaks. Returns
    /// the admission-control verdict iff a ceiling *first* trips on this tick, so
    /// the caller can kill the group exactly once. Idempotent afterwards.
    pub fn sample(&mut self) -> Option<AdmissionControl> {
        let mut landed = false;

        if let Some(rss) = self
            .probe
            .ps_pid_pgid_rss()
            .and_then(|text| parse_managed_rss_bytes(&text, self.supervisor_pid, self.child_pgid))
        {
            landed = true;
            self.peak_managed_rss_bytes =
                Some(self.peak_managed_rss_bytes.map_or(rss, |p| p.max(rss)));
        }

        if let Some(frac) = self.sample_host_mem_frac() {
            landed = true;
            self.peak_host_mem_used_frac =
                Some(self.peak_host_mem_used_frac.map_or(frac, |p| p.max(frac)));
        }

        if let Some(swap) = self
            .probe
            .swap_usage()
            .and_then(|text| parse_swap_used_bytes(&text))
        {
            landed = true;
            self.peak_host_swap_used_bytes =
                Some(self.peak_host_swap_used_bytes.map_or(swap, |p| p.max(swap)));
        }

        if landed {
            self.samples = self.samples.saturating_add(1);
        }

        // Admission control fires at most once (first crossing wins).
        if self.admission_control.is_none() {
            if let Some(ac) = self.evaluate_guards() {
                self.admission_control = Some(ac.clone());
                return Some(ac);
            }
        }
        None
    }

    /// Prefer the macOS `vm_stat` + `hw.memsize` path; fall back to Linux
    /// `/proc/meminfo`. Returns a `[0,1]` used-fraction or `None`.
    fn sample_host_mem_frac(&self) -> Option<f64> {
        if let (Some(vm), Some(total)) = (self.probe.vm_stat(), self.probe.mem_total_bytes()) {
            if let Some(frac) = mem_used_frac_from_vm_stat(&vm, total) {
                return Some(frac);
            }
        }
        self.probe
            .meminfo()
            .and_then(|text| mem_used_frac_from_meminfo(&text))
    }

    fn evaluate_guards(&self) -> Option<AdmissionControl> {
        if let (Some(limit), Some(observed)) =
            (self.guards.max_proc_rss_bytes, self.peak_managed_rss_bytes)
        {
            if observed > limit {
                return Some(AdmissionControl {
                    reason: "resource_proc_rss".to_string(),
                    observed,
                    limit,
                });
            }
        }
        if let (Some(limit_pct), Some(frac)) = (
            self.guards.host_mem_pressure_abort_pct,
            self.peak_host_mem_used_frac,
        ) {
            let observed_pct = (frac * 100.0).round() as u64;
            if observed_pct > limit_pct {
                return Some(AdmissionControl {
                    reason: "resource_host_pressure".to_string(),
                    observed: observed_pct,
                    limit: limit_pct,
                });
            }
        }
        None
    }

    /// Freeze the accumulated observations into an immutable [`RunResources`].
    /// When nothing ever landed and no guard tripped, returns `None` so the
    /// telemetry field itself is `null` (honest absence over an empty shell).
    pub fn snapshot(&self) -> Option<RunResources> {
        if self.samples == 0
            && self.peak_managed_rss_bytes.is_none()
            && self.peak_host_mem_used_frac.is_none()
            && self.peak_host_swap_used_bytes.is_none()
            && self.admission_control.is_none()
        {
            return None;
        }
        Some(RunResources {
            peak_managed_rss_bytes: self.peak_managed_rss_bytes,
            peak_host_mem_used_frac: self.peak_host_mem_used_frac,
            peak_host_mem_level: self.peak_host_mem_used_frac.map(HostMemLevel::classify),
            peak_host_swap_used_bytes: self.peak_host_swap_used_bytes,
            samples: self.samples,
            admission_control: self.admission_control.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Pure parsers (hermetically unit-tested below)
// ---------------------------------------------------------------------------

/// Sum RSS for the supervisor PID and every member of its isolated child PGID.
/// A PID is counted once even if both predicates match. Input is
/// `ps -Ao pid=,pgid=,rss=` output where RSS is KiB.
pub fn parse_managed_rss_bytes(
    ps_output: &str,
    supervisor_pid: i32,
    child_pgid: i32,
) -> Option<u64> {
    let mut total_kib: u64 = 0;
    let mut matched = false;
    for line in ps_output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(pgid), Some(rss)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(pgid), Ok(rss)) =
            (pid.parse::<i32>(), pgid.parse::<i32>(), rss.parse::<u64>())
        else {
            continue;
        };
        if pid == supervisor_pid || pgid == child_pgid {
            matched = true;
            total_kib = total_kib.saturating_add(rss);
        }
    }
    if matched {
        Some(total_kib.saturating_mul(1024))
    } else {
        None
    }
}

/// Parse macOS `vm.swapusage` into swap-used bytes. Line shape:
/// `total = 2048.00M  used = 512.50M  free = 1535.50M  (encrypted)`.
pub fn parse_swap_used_bytes(text: &str) -> Option<u64> {
    let idx = text.find("used")?;
    let rest = &text[idx + "used".len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let token: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    parse_size_token(&token)
}

/// Parse a `123.45M` / `1.00G` / `512K` / `4096` size token into bytes.
fn parse_size_token(token: &str) -> Option<u64> {
    if token.is_empty() {
        return None;
    }
    let (num_part, mult) = match token.chars().last().unwrap() {
        'K' | 'k' => (&token[..token.len() - 1], 1024.0_f64),
        'M' | 'm' => (&token[..token.len() - 1], 1024.0 * 1024.0),
        'G' | 'g' => (&token[..token.len() - 1], 1024.0 * 1024.0 * 1024.0),
        'T' | 't' => (&token[..token.len() - 1], 1024.0 * 1024.0 * 1024.0 * 1024.0),
        '0'..='9' => (token, 1.0),
        _ => return None,
    };
    let value: f64 = num_part.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some((value * mult) as u64)
}

/// Compute host memory used-fraction from macOS `vm_stat` output and the total
/// physical memory (bytes). "Available" is approximated as free + inactive +
/// speculative pages (pages the kernel can reclaim without swapping), matching
/// how Activity Monitor treats them as non-pressure memory.
pub fn mem_used_frac_from_vm_stat(vm_stat: &str, total_bytes: u64) -> Option<f64> {
    if total_bytes == 0 {
        return None;
    }
    let page_size = vm_stat_page_size(vm_stat)?;
    let free = vm_stat_pages(vm_stat, "Pages free")?;
    let inactive = vm_stat_pages(vm_stat, "Pages inactive").unwrap_or(0);
    let speculative = vm_stat_pages(vm_stat, "Pages speculative").unwrap_or(0);
    let available_pages = free.saturating_add(inactive).saturating_add(speculative);
    let available_bytes = available_pages.saturating_mul(page_size);
    let used = total_bytes.saturating_sub(available_bytes);
    let frac = used as f64 / total_bytes as f64;
    Some(frac.clamp(0.0, 1.0))
}

fn vm_stat_page_size(vm_stat: &str) -> Option<u64> {
    // Header: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let marker = "page size of ";
    let idx = vm_stat.find(marker)?;
    let rest = &vm_stat[idx + marker.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok().filter(|v| *v > 0)
}

fn vm_stat_pages(vm_stat: &str, label: &str) -> Option<u64> {
    for line in vm_stat.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(label) {
            let rest = rest.trim_start_matches(':').trim();
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            return digits.parse::<u64>().ok();
        }
    }
    None
}

/// Compute host memory used-fraction from Linux `/proc/meminfo` using
/// `MemTotal` and `MemAvailable` (both reported in KiB).
pub fn mem_used_frac_from_meminfo(meminfo: &str) -> Option<f64> {
    let total = meminfo_kib(meminfo, "MemTotal")?;
    let available = meminfo_kib(meminfo, "MemAvailable")?;
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(available);
    Some((used as f64 / total as f64).clamp(0.0, 1.0))
}

fn meminfo_kib(meminfo: &str, key: &str) -> Option<u64> {
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            let rest = rest.trim_start_matches(':').trim();
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            return digits.parse::<u64>().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn managed_rss_sums_supervisor_and_child_group_once() {
        let ps = "\
10 10 40
20 4242 100
21 4242 200
99 9999 500
";
        let bytes = parse_managed_rss_bytes(ps, 10, 4242).unwrap();
        assert_eq!(bytes, (40 + 100 + 200) * 1024);
    }

    #[test]
    fn managed_rss_none_when_family_absent() {
        let ps = "1 1 10\n2 2 20\n";
        assert_eq!(parse_managed_rss_bytes(ps, 10, 4242), None);
    }

    #[test]
    fn managed_rss_skips_garbage_lines() {
        let ps = "header junk\n20 4242 abc\n20 4242 128\n\n";
        assert_eq!(parse_managed_rss_bytes(ps, 10, 4242), Some(128 * 1024));
    }

    #[test]
    fn swap_used_parses_megabytes() {
        let line = "total = 2048.00M  used = 512.50M  free = 1535.50M  (encrypted)";
        let bytes = parse_swap_used_bytes(line).unwrap();
        assert_eq!(bytes, (512.5 * 1024.0 * 1024.0) as u64);
    }

    #[test]
    fn swap_used_parses_gigabytes_and_zero() {
        assert_eq!(
            parse_swap_used_bytes("used = 1.00G"),
            Some(1024 * 1024 * 1024)
        );
        assert_eq!(parse_swap_used_bytes("used = 0.00M"), Some(0));
    }

    #[test]
    fn swap_used_none_on_garbage() {
        assert_eq!(parse_swap_used_bytes("nonsense here"), None);
        assert_eq!(parse_swap_used_bytes("used = "), None);
    }

    #[test]
    fn size_token_bare_number_is_bytes() {
        assert_eq!(parse_size_token("4096"), Some(4096));
        assert_eq!(parse_size_token("512K"), Some(512 * 1024));
        assert_eq!(parse_size_token(""), None);
        assert_eq!(parse_size_token("M"), None);
    }

    #[test]
    fn vm_stat_used_fraction_reasonable() {
        // page size 16384; total 32 pages worth = 32*16384 bytes.
        // free=4 inactive=2 speculative=2 => available 8 pages => used 24/32.
        let vm = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                          4.
Pages active:                        16.
Pages inactive:                      2.
Pages speculative:                   2.
Pages wired down:                    8.
";
        let total = 32 * 16384;
        let frac = mem_used_frac_from_vm_stat(vm, total).unwrap();
        assert!((frac - (24.0 / 32.0)).abs() < 1e-9, "frac={frac}");
    }

    #[test]
    fn vm_stat_none_without_page_size() {
        let vm = "Pages free: 10.\n";
        assert_eq!(mem_used_frac_from_vm_stat(vm, 1024), None);
    }

    #[test]
    fn vm_stat_none_on_zero_total() {
        let vm = "page size of 4096 bytes\nPages free: 1.\n";
        assert_eq!(mem_used_frac_from_vm_stat(vm, 0), None);
    }

    #[test]
    fn meminfo_used_fraction_reasonable() {
        let meminfo = "\
MemTotal:       16000000 kB
MemFree:         1000000 kB
MemAvailable:    4000000 kB
Buffers:          200000 kB
";
        let frac = mem_used_frac_from_meminfo(meminfo).unwrap();
        assert!(
            (frac - (12000000.0 / 16000000.0)).abs() < 1e-9,
            "frac={frac}"
        );
    }

    #[test]
    fn meminfo_none_when_field_missing() {
        assert_eq!(mem_used_frac_from_meminfo("MemTotal: 100 kB\n"), None);
    }

    #[test]
    fn host_mem_level_bands() {
        assert_eq!(HostMemLevel::classify(0.10), HostMemLevel::Normal);
        assert_eq!(HostMemLevel::classify(0.85), HostMemLevel::Warn);
        assert_eq!(HostMemLevel::classify(0.90), HostMemLevel::Warn);
        assert_eq!(HostMemLevel::classify(0.95), HostMemLevel::Critical);
        assert_eq!(HostMemLevel::classify(1.00), HostMemLevel::Critical);
    }

    // ----- Sampler behaviour with a fully hermetic fake probe -----

    #[derive(Default)]
    struct FakeProbe {
        ps_lines: RefCell<Vec<Option<String>>>,
        vm: Option<String>,
        total: Option<u64>,
        swap: RefCell<Vec<Option<String>>>,
        meminfo: Option<String>,
    }

    impl ResourceProbe for FakeProbe {
        fn ps_pid_pgid_rss(&self) -> Option<String> {
            let mut q = self.ps_lines.borrow_mut();
            if q.is_empty() {
                None
            } else {
                q.remove(0)
            }
        }
        fn vm_stat(&self) -> Option<String> {
            self.vm.clone()
        }
        fn mem_total_bytes(&self) -> Option<u64> {
            self.total
        }
        fn swap_usage(&self) -> Option<String> {
            let mut q = self.swap.borrow_mut();
            if q.is_empty() {
                None
            } else {
                q.remove(0)
            }
        }
        fn meminfo(&self) -> Option<String> {
            self.meminfo.clone()
        }
    }

    #[test]
    fn sampler_tracks_peak_rss_across_ticks() {
        let probe = FakeProbe {
            ps_lines: RefCell::new(vec![
                Some("1 1 10\n7 7 100\n".to_string()), // 110 KiB
                Some("1 1 10\n7 7 300\n".to_string()), // 310 KiB (peak)
                Some("1 1 10\n7 7 200\n".to_string()), // back down
            ]),
            ..Default::default()
        };
        let mut s = ResourceSampler::new(probe, 1, 7, ResourceGuards::default());
        assert!(s.sample().is_none());
        assert!(s.sample().is_none());
        assert!(s.sample().is_none());
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.peak_managed_rss_bytes, Some(310 * 1024));
        assert_eq!(snap.samples, 3);
        assert!(snap.admission_control.is_none());
    }

    #[test]
    fn sampler_snapshot_none_when_all_probes_fail() {
        let probe = FakeProbe::default(); // every read returns None
        let mut s = ResourceSampler::new(probe, 1, 7, ResourceGuards::default());
        assert!(s.sample().is_none());
        assert!(s.snapshot().is_none(), "no data must yield null telemetry");
    }

    #[test]
    fn sampler_rss_ceiling_trips_once() {
        let probe = FakeProbe {
            ps_lines: RefCell::new(vec![
                Some("7 7 100\n".to_string()), // 100 KiB < limit
                Some("7 7 900\n".to_string()), // 900 KiB > limit -> trip
                Some("7 7 950\n".to_string()), // already tripped -> no second fire
            ]),
            ..Default::default()
        };
        let guards = ResourceGuards {
            max_proc_rss_bytes: Some(500 * 1024),
            host_mem_pressure_abort_pct: None,
        };
        let mut s = ResourceSampler::new(probe, 1, 7, guards);
        assert!(s.sample().is_none());
        let ac = s.sample().expect("ceiling should trip on 2nd tick");
        assert_eq!(ac.reason, "resource_proc_rss");
        assert_eq!(ac.observed, 900 * 1024);
        assert_eq!(ac.limit, 500 * 1024);
        assert!(s.sample().is_none(), "must not re-fire after first trip");
        let snap = s.snapshot().unwrap();
        assert_eq!(
            snap.admission_control.as_ref().map(|a| a.reason.as_str()),
            Some("resource_proc_rss")
        );
    }

    #[test]
    fn sampler_host_pressure_ceiling_trips() {
        // vm_stat with 0 available pages -> used fraction 1.0 -> 100%.
        let vm = "\
Mach Virtual Memory Statistics: (page size of 4096 bytes)
Pages free:                          0.
Pages inactive:                      0.
Pages speculative:                   0.
Pages wired down:                    10.
";
        let probe = FakeProbe {
            vm: Some(vm.to_string()),
            total: Some(10 * 4096),
            ..Default::default()
        };
        let guards = ResourceGuards {
            max_proc_rss_bytes: None,
            host_mem_pressure_abort_pct: Some(90),
        };
        let mut s = ResourceSampler::new(probe, 1, 7, guards);
        let ac = s.sample().expect("host pressure should trip");
        assert_eq!(ac.reason, "resource_host_pressure");
        assert_eq!(ac.observed, 100);
        assert_eq!(ac.limit, 90);
    }

    #[test]
    fn sampler_folds_swap_and_host_mem() {
        let vm = "\
Mach Virtual Memory Statistics: (page size of 4096 bytes)
Pages free:                          5.
Pages inactive:                      0.
Pages speculative:                   0.
Pages wired down:                    5.
";
        let probe = FakeProbe {
            vm: Some(vm.to_string()),
            total: Some(10 * 4096),
            swap: RefCell::new(vec![Some("used = 2.00M".to_string())]),
            ..Default::default()
        };
        let mut s = ResourceSampler::new(probe, 1, 7, ResourceGuards::default());
        assert!(s.sample().is_none());
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.peak_host_swap_used_bytes, Some(2 * 1024 * 1024));
        assert_eq!(snap.peak_host_mem_used_frac, Some(0.5));
        assert_eq!(snap.peak_host_mem_level, Some(HostMemLevel::Normal));
        assert_eq!(snap.samples, 1);
    }
}
