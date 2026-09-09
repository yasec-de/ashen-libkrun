// SPDX-License-Identifier: Apache-2.0

//! Describing guest RAM to the host's kdump kernel.
//!
//! A guest's memory is not the host's to hand to whoever ends up with the
//! host's vmcore, but it is also by far the largest thing in that dump. Linux
//! (`CONFIG_CRASH_MEMACTION`) lets a process say what a range of its own memory
//! holds -- secrets, or nothing worth dumping -- and leaves what to do about it
//! to the kdump kernel's policy.
//!
//! We describe the whole of the `GuestMemoryMmap` right after it is created,
//! before the payload is loaded and before any vCPU runs. Nothing is pinned or
//! populated by doing so: the mark lives in the VMA flags, so pages that are
//! faulted in later are covered without another call, and a VM configured with
//! 64 GiB of RAM still only commits what the guest touches.
//!
//! A type the host kernel does not track is dropped by `madvise()` rather than
//! refused, so there is nothing here to detect and nothing to fall back to:
//! `/proc/pid/smaps` flags the mappings that were really marked with `cm`.

use std::io;

/// The pages hold secrets.
pub const CRASH_MEMACTION_SECRET: u32 = 1 << 0;
/// The pages hold nothing worth dumping.
pub const CRASH_MEMACTION_CACHE: u32 = 1 << 1;

/// Every type this build understands.
pub const CRASH_MEMACTION_MASK: u32 = CRASH_MEMACTION_SECRET | CRASH_MEMACTION_CACHE;

/// `MADV_CRASH_SECRET`, from `include/uapi/asm-generic/mman-common.h`.
#[cfg(target_os = "linux")]
const MADV_CRASH_SECRET: i32 = 26;
/// `MADV_CRASH_CACHE`.
#[cfg(target_os = "linux")]
const MADV_CRASH_CACHE: i32 = 27;

/// What this microVM's guest RAM holds, as the host's kdump kernel sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrashMemactionConfig {
    types: u32,
}

impl CrashMemactionConfig {
    /// Build a configuration from the caller's type bits, or `None` if they
    /// name a type this build does not know about.
    pub fn new(types: u32) -> Option<Self> {
        (types & !CRASH_MEMACTION_MASK == 0).then_some(Self { types })
    }

    /// Whether anything is to be described at all.
    pub fn is_empty(&self) -> bool {
        self.types == 0
    }

    /// The `madvise()` behaviours this configuration asks for.
    #[cfg(target_os = "linux")]
    fn behaviors(&self) -> Vec<i32> {
        let mut behaviors = Vec::with_capacity(2);
        if self.types & CRASH_MEMACTION_SECRET != 0 {
            behaviors.push(MADV_CRASH_SECRET);
        }
        if self.types & CRASH_MEMACTION_CACHE != 0 {
            behaviors.push(MADV_CRASH_CACHE);
        }
        behaviors
    }

    /// Describe one host memory range according to this configuration.
    ///
    /// # Safety
    ///
    /// `addr` must point at `len` bytes of a live anonymous, shmem or hugetlb
    /// mapping owned by this process. The marks change nothing about the
    /// mapping's contents or lifetime, so an over-long range is a correctness
    /// bug rather than a memory-safety one, but the pointer still has to be one
    /// `madvise()` may be handed.
    #[cfg(target_os = "linux")]
    pub unsafe fn mark_range(&self, addr: *mut u8, len: usize) -> io::Result<()> {
        for behavior in self.behaviors() {
            // SAFETY: the caller guarantees addr/len describe a mapping of
            // ours. madvise() with these behaviours neither reads nor writes
            // the range, it only sets a flag on the VMA.
            if unsafe { libc::madvise(addr as *mut libc::c_void, len, behavior) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Non-Linux hosts have no equivalent.
    #[cfg(not(target_os = "linux"))]
    pub unsafe fn mark_range(&self, _addr: *mut u8, _len: usize) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_types() {
        assert!(CrashMemactionConfig::new(1 << 5).is_none());
    }

    #[test]
    fn empty_unless_a_type_is_asked_for() {
        assert!(CrashMemactionConfig::new(0).unwrap().is_empty());
        assert!(
            !CrashMemactionConfig::new(CRASH_MEMACTION_SECRET)
                .unwrap()
                .is_empty()
        );
        assert!(
            !CrashMemactionConfig::new(CRASH_MEMACTION_CACHE)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn behaviors_follow_the_type_bits() {
        let cfg =
            CrashMemactionConfig::new(CRASH_MEMACTION_SECRET | CRASH_MEMACTION_CACHE).unwrap();
        assert_eq!(cfg.behaviors(), vec![MADV_CRASH_SECRET, MADV_CRASH_CACHE]);
        let cfg = CrashMemactionConfig::new(CRASH_MEMACTION_CACHE).unwrap();
        assert_eq!(cfg.behaviors(), vec![MADV_CRASH_CACHE]);
    }
}
