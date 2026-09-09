//! Reading the crash marks back out of the kernel.
//!
//! `krun_set_crash_memaction` succeeding only means `madvise()` returned 0, and
//! under this ABI that says less than it looks: a kernel that does not track
//! the type we asked for drops the request and still returns 0. What it does
//! not do is set the VMA flag, so `/proc/self/smaps` is the one place that
//! distinguishes a mark that took from one that was quietly dropped.
//!
//! The kernel reports the flag in the `VmFlags:` line as `cm` (`VM_CRASH_MARK`).

use anyhow::{Context, Result};
use std::fmt;
use std::fs;

/// What `/proc/self/smaps` says about the crash marks on our mappings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    /// Total size of mappings flagged `cm`, in KiB.
    pub marked_kib: u64,
    /// Resident pages within marked mappings, in KiB. Guest RAM is marked but
    /// not populated, so this is how much the guest has actually touched.
    pub resident_kib: u64,
    /// Number of marked VMAs.
    pub vmas: usize,
}

impl fmt::Display for Marks {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} VMA(s): {} MiB marked, {} MiB resident",
            self.vmas,
            self.marked_kib / 1024,
            self.resident_kib / 1024,
        )
    }
}

impl Marks {
    pub fn is_empty(&self) -> bool {
        self.vmas == 0
    }
}

/// Parse `/proc/self/smaps` and total the crash-marked mappings.
pub fn read_marks() -> Result<Marks> {
    let smaps = fs::read_to_string("/proc/self/smaps").context("reading /proc/self/smaps")?;
    Ok(parse_smaps(&smaps))
}

/// Total the crash-marked VMAs in the text of a smaps file.
///
/// smaps is a sequence of records: a header line naming the address range,
/// then `Key: value kB` lines, ending with `VmFlags:`. The flags come last, so
/// each record's size and Rss are buffered until we know whether it is marked.
fn parse_smaps(smaps: &str) -> Marks {
    let mut marks = Marks::default();
    let mut size_kib = 0u64;
    let mut rss_kib = 0u64;

    for line in smaps.lines() {
        if let Some(value) = line.strip_prefix("Size:") {
            size_kib = parse_kib(value);
        } else if let Some(value) = line.strip_prefix("Rss:") {
            rss_kib = parse_kib(value);
        } else if let Some(value) = line.strip_prefix("VmFlags:") {
            if has_flag(value, "cm") {
                marks.vmas += 1;
                marks.marked_kib += size_kib;
                marks.resident_kib += rss_kib;
            }
            size_kib = 0;
            rss_kib = 0;
        }
    }

    marks
}

/// `VmFlags:` is a space-separated list of two-letter tokens. Match whole
/// tokens: a substring test would confuse `cm` with, say, a future `cmt`.
fn has_flag(vm_flags: &str, flag: &str) -> bool {
    vm_flags.split_whitespace().any(|token| token == flag)
}

/// Values look like ` 524288 kB`.
fn parse_kib(value: &str) -> u64 {
    value
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
ffff8000-ffff9000 rw-p 00000000 00:00 0
Size:                  4 kB
Rss:                   4 kB
VmFlags: rd wr mr mw me ac
40000000-60000000 rw-p 00000000 00:00 0
Size:             524288 kB
Rss:                2048 kB
VmFlags: rd wr mr mw me ac cm
60000000-61000000 rw-s 00000000 00:00 0
Size:              16384 kB
Rss:                 512 kB
VmFlags: rd wr sh mr mw me ac cm
";

    #[test]
    fn totals_only_marked_vmas() {
        let marks = parse_smaps(SAMPLE);
        assert_eq!(marks.vmas, 2);
        assert_eq!(marks.marked_kib, 524288 + 16384);
        // The unmarked 4 kB VMA contributes nothing.
        assert_eq!(marks.resident_kib, 2048 + 512);
    }

    #[test]
    fn no_marks_is_empty() {
        let marks = parse_smaps(
            "40000000-60000000 rw-p 00000000 00:00 0 \nSize: 4 kB\nRss: 4 kB\nVmFlags: rd wr\n",
        );
        assert!(marks.is_empty());
        assert_eq!(marks, Marks::default());
    }

    #[test]
    fn flags_match_whole_tokens() {
        assert!(has_flag("rd wr cm", "cm"));
        assert!(!has_flag("rd wr cmt", "cm"));
        assert!(!has_flag("rd wr", "cm"));
    }
}
