#!/usr/bin/env python3
"""Patch zip 0.6.6 (corelib dependency) so its fallback AtomicU64 is used on Xtensa.

zip 0.6.6's cfg only lists arm32/mips/powerpc as lacking 64-bit atomics;
Xtensa (ESP32-S3) is missing, so it tries to use std's AtomicU64 which does
not exist on this target. We add xtensa to the cfg AND rewrite the fallback
mod atomic to use std::sync::Mutex instead of crossbeam-utils (whose target
dependency was resolved before our patch, so it would not be in the lock
file).
"""
import sys


def main():
    if len(sys.argv) < 2:
        print("usage: patch_zip.py <zip_dir>")
        sys.exit(1)
    d = sys.argv[1]

    p = d + "/src/types.rs"
    s = open(p, encoding="utf-8").read()

    # 1) add xtensa to the native-atomic exclusion cfg
    old1 = """#[cfg(not(any(
    all(target_arch = "arm", target_pointer_width = "32"),
    target_arch = "mips",
    target_arch = "powerpc"
)))]
use std::sync::atomic;"""
    new1 = """#[cfg(not(any(
    all(target_arch = "arm", target_pointer_width = "32"),
    target_arch = "mips",
    target_arch = "powerpc",
    target_arch = "xtensa"
)))]
use std::sync::atomic;"""
    if old1 in s:
        s = s.replace(old1, new1)
        print("types.rs: patched native cfg")
    else:
        print("types.rs: native cfg pattern not found (already patched?)")

    # 2) add xtensa to the fallback cfg
    old2 = """#[cfg(any(
    all(target_arch = "arm", target_pointer_width = "32"),
    target_arch = "mips",
    target_arch = "powerpc"
))]
mod atomic {"""
    new2 = """#[cfg(any(
    all(target_arch = "arm", target_pointer_width = "32"),
    target_arch = "mips",
    target_arch = "powerpc",
    target_arch = "xtensa"
))]
mod atomic {"""
    if old2 in s:
        s = s.replace(old2, new2)
        print("types.rs: patched fallback cfg")
    else:
        print("types.rs: fallback cfg pattern not found (already patched?)")

    # 3) rewrite fallback mod atomic to use std::sync::Mutex
    old_atomic = """mod atomic {
    use crossbeam_utils::sync::ShardedLock;
    pub use std::sync::atomic::Ordering;

    #[derive(Debug, Default)]
    pub struct AtomicU64 {
        value: ShardedLock<u64>,
    }

    impl AtomicU64 {
        pub fn new(v: u64) -> Self {
            Self {
                value: ShardedLock::new(v),
            }
        }
        pub fn get_mut(&mut self) -> &mut u64 {
            self.value.get_mut().unwrap()
        }
        pub fn load(&self, _: Ordering) -> u64 {
            *self.value.read().unwrap()
        }
        pub fn store(&self, value: u64, _: Ordering) {
            *self.value.write().unwrap() = value;
        }
    }
}"""
    new_atomic = """mod atomic {
    use std::sync::Mutex;
    pub use std::sync::atomic::Ordering;

    #[derive(Debug, Default)]
    pub struct AtomicU64 {
        value: Mutex<u64>,
    }

    impl AtomicU64 {
        pub fn new(v: u64) -> Self {
            Self {
                value: Mutex::new(v),
            }
        }
        pub fn get_mut(&mut self) -> &mut u64 {
            self.value.get_mut().unwrap()
        }
        pub fn load(&self, _: Ordering) -> u64 {
            *self.value.lock().unwrap()
        }
        pub fn store(&self, value: u64, _: Ordering) {
            *self.value.lock().unwrap() = value;
        }
    }
}"""
    if old_atomic in s:
        s = s.replace(old_atomic, new_atomic)
        print("types.rs: fallback atomic rewritten with std Mutex")
    else:
        print("types.rs: fallback atomic pattern not found (already patched?)")
    open(p, "w", encoding="utf-8").write(s)
    print("Cargo.toml: left untouched (crossbeam-utils no longer used)")


if __name__ == "__main__":
    main()
