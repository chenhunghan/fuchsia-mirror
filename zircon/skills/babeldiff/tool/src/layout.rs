//! Where C++ is structurally expected to be for a given Rust file. The C++
//! a Rust function replaces is normally in the same component (the
//! directory the Rust crate lives in) or in a directory above it, and never
//! in another architecture's directory: Rust under `arch/x86` doesn't port
//! C++ under `arch/riscv64`.

/// Directory names that make a path specific to one architecture.
const ARCHES: &[&str] = &[
    "x86", "x64", "x86_64", "amd64", "i386", "arm64", "aarch64", "riscv", "riscv64",
];

/// Names that stand for an architecture only right after an `arch`
/// directory, as in `lib/arch/host/` or `arch/arm/`.
const ARCH_ONLY_UNDER_ARCH: &[&str] = &["arm", "host"];

fn dirs(path: &str) -> Vec<&str> {
    let mut v: Vec<&str> = path.split('/').collect();
    v.pop();
    v
}

/// The architectures a path is specific to, by its directories.
pub fn arches(path: &str) -> Vec<&str> {
    let ds = dirs(path);
    let mut out = Vec::new();
    for (i, d) in ds.iter().enumerate() {
        let under_arch = i > 0 && ds[i - 1] == "arch";
        if ARCHES.contains(d) || (under_arch && ARCH_ONLY_UNDER_ARCH.contains(d)) {
            out.push(*d);
        }
    }
    out
}

/// Whether two paths belong to different architectures.
pub fn arch_conflict(a: &str, b: &str) -> bool {
    let (x, y) = (arches(a), arches(b));
    !x.is_empty() && !y.is_empty() && !x.iter().any(|a| y.contains(a))
}

/// The component a Rust file belongs to: its directory without trailing
/// `src` and `rust` directories (`arch/x86/src/mp.rs` is in `arch/x86`).
fn component(rust: &str) -> String {
    let mut ds = dirs(rust);
    if let Some(i) = ds.iter().position(|d| *d == "src") {
        ds.truncate(i);
    }
    while ds.last().is_some_and(|d| matches!(*d, "src" | "rust")) {
        ds.pop();
    }
    ds.join("/")
}

/// Whether C++ at `cpp` is where the C++ for Rust at `rust` is expected:
/// inside the Rust's component, in a directory above it (such as
/// `arch/arch_ffi.cc` for `arch/x86`), or in a directory for the same
/// architecture (`lib/arch/x86/`).
pub fn related(cpp: &str, rust: &str) -> bool {
    if arch_conflict(cpp, rust) {
        return false;
    }
    let comp = component(rust);
    let cdir = dirs(cpp).join("/");
    if comp.is_empty() || cdir.is_empty() {
        return true;
    }
    let under = |p: &str, dir: &str| p == dir || p.starts_with(&format!("{dir}/"));
    under(&cdir, &comp)
        || under(&comp, &cdir)
        || (!arches(rust).is_empty() && !arches(cpp).is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn architectures() {
        assert_eq!(arches("k/arch/x86/src/mp.rs"), ["x86"]);
        assert_eq!(
            arches("k/lib/arch/host/include/lib/arch/intrin.h"),
            ["host"]
        );
        assert!(arches("k/host/tool.cc").is_empty());
        assert!(arch_conflict(
            "k/arch/x86/src/mmu.rs",
            "k/arch/riscv64/include/arch/aspace.h"
        ));
        assert!(arch_conflict(
            "k/arch/x86/src/x86.rs",
            "k/lib/arch/host/include/lib/arch/intrin.h"
        ));
        assert!(!arch_conflict(
            "k/arch/x86/src/mmu.rs",
            "k/lib/arch/include/lib/arch/x86/bug.h"
        ));
        assert!(!arch_conflict("k/arch/x86/src/mmu.rs", "k/vm/vm.cc"));
    }

    #[test]
    fn related_directories() {
        let rust = "k/arch/x86/src/mp.rs";
        assert!(related("k/arch/x86/mp.cc", rust));
        assert!(related("k/arch/x86/include/arch/x86/mp.h", rust));
        assert!(related("k/arch/arch_ffi.cc", rust));
        assert!(related("k/lib/arch/include/lib/arch/x86/bug.h", rust));
        assert!(!related("k/arch/arm64/mmu.cc", rust));
        assert!(!related("k/vm/include/vm/vm_object.h", rust));
        assert!(related(
            "k/lib/counters/counters.cc",
            "k/lib/counters/rust/src/lib.rs"
        ));
    }
}
