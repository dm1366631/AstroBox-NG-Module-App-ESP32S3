#!/usr/bin/env python3
"""Patch zip 0.6.6 (corelib dependency) so its fallback AtomicU64 is used on Xtensa.

zip 0.6.6's cfg only lists arm32/mips/powerpc as lacking 64-bit atomics;
Xtensa (ESP32-S3) is missing, so it tries to use std's AtomicU64 which does
not exist on this target. Adding xtensa to the cfg makes it use the
crossbeam-utils-backed fallback (u64 semantics unchanged).
"""
import sys


def main():
    if len(sys.argv) < 2:
        print("usage: patch_zip.py <zip_dir>")
        sys.exit(1)
    d = sys.argv[1]

    # --- types.rs: add xtensa to both cfg conditions ---
    p = d + "/src/types.rs"
    s = open(p, encoding="utf-8").read()
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
    open(p, "w", encoding="utf-8").write(s)

    # --- Cargo.toml: add xtensa to crossbeam-utils target dependency ---
    p = d + "/Cargo.toml"
    s = open(p, encoding="utf-8").read()
    old3 = 'target_arch = \\"powerpc\\"))".dependencies.crossbeam-utils]'
    new3 = 'target_arch = \\"powerpc\\", target_arch = \\"xtensa\\"))".dependencies.crossbeam-utils]'
    if old3 in s:
        s = s.replace(old3, new3)
        print("Cargo.toml: patched crossbeam-utils target")
    else:
        print("Cargo.toml: crossbeam pattern not found (already patched?)")
    open(p, "w", encoding="utf-8").write(s)


if __name__ == "__main__":
    main()
