// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use counters_abi::{Descriptor, DescriptorVmo, Type};
use kcounter_lib::vmo::{
    ARENA_VMO_FILE, Arena, ArenaError, DESCRIPTOR_VMO_FILE, Descriptors, DescriptorsError,
};
use kcounter_lib::{KcounterCmdline, kcounter_parse_cmdline};
use std::fs::File;
use std::mem::size_of;
use zerocopy::IntoBytes;

const VMO_FILE_PREFIX: &str = "/boot/kernel/";

/// Opens `VMO_FILE_PREFIX/name`, returning its VMO and the VMO's size.
fn open_vmo(name: &str, what: &str) -> (zx::Vmo, u64) {
    let file = File::open(format!("{VMO_FILE_PREFIX}{name}"))
        .unwrap_or_else(|e| panic!("cannot open {what} VMO file: {e}"));
    let vmo = fdio::get_vmo_exact_from_file(&file).expect("fdio_get_vmo_exact");
    let size = vmo.get_size().expect("get_size");
    (vmo, size)
}

#[test]
fn counters_basic() {
    let (vmo, size) = open_vmo(DESCRIPTOR_VMO_FILE, "descriptor");
    // `Descriptors::from_vmo()` checks the magic number and that the size holds
    // the descriptor table.
    let desc =
        Descriptors::from_vmo(&vmo, size).expect("descriptor VMO magic number and table size");
    drop(vmo);

    let (vmo, size) = open_vmo(ARENA_VMO_FILE, "arena");
    assert!(size >= desc.arena_size().expect("arena size overflow"), "arena VMO size");
    // SAFETY: `/boot/kernel/counters/arena` is updated concurrently by the
    // kernel exclusively via aligned 64-bit atomic operations (without
    // page-replacing VMO ops or cache-policy changes), and nothing modifies the
    // resulting mapping in `vmar_root_self()` while `arena` is live.
    let arena = unsafe { Arena::map(&vmo, size, &desc) }.expect("map arena VMO");
    drop(vmo);

    let find = |name: &[u8]| -> Option<usize> {
        desc.table().binary_search_by(|entry| entry.name_bytes().cmp(name)).ok()
    };

    const EXPECTED: [(&[u8], Type); 4] = [
        (b"init.target.time.msec", Type::Sum),
        (b"handles.duped", Type::Sum),
        (b"handles.live", Type::Sum),
        (b"handles.made", Type::Sum),
    ];
    let mut failures = Vec::new();
    for (name, ref_type) in EXPECTED {
        let name_str = String::from_utf8_lossy(name);
        let Some(idx) = find(name) else {
            failures.push(format!("{name_str}: expected counter name not found"));
            continue;
        };
        let found_type = Type::try_from(desc.table()[idx].type_);
        if found_type != Ok(ref_type) {
            failures.push(format!("{name_str}: counter has wrong type {found_type:?}"));
        }
        let mut value: i64 = 0;
        for cpu in 0..desc.header().max_cpus {
            let cpu_value = arena.get(cpu, idx);
            match ref_type {
                Type::Sum => value = value.wrapping_add(cpu_value),
                // Not used, see https://fxbug.dev/42108304.
                Type::Max => value = value.max(cpu_value),
                _ => panic!("unexpected counter type {ref_type:?}"),
            }
        }
        if value <= 0 {
            failures.push(format!("{name_str}: value {value} is not > 0"));
        }
    }
    // Report every failure rather than stopping at the first.
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Builds a synthetic descriptor VMO populated with `header` and `entries`.
fn make_desc_vmo(header: &DescriptorVmo, entries: &[Descriptor]) -> zx::Vmo {
    let total = size_of::<DescriptorVmo>() + entries.as_bytes().len();
    let vmo = zx::Vmo::create(total as u64).expect("create descriptor VMO");
    vmo.write(header.as_bytes(), 0).expect("write header");
    vmo.write(entries.as_bytes(), size_of::<DescriptorVmo>() as u64).expect("write descriptors");
    vmo
}

/// Builds a [`Descriptor`] with `name` zero-padded to 56 bytes and `type_`.
fn make_descriptor(name: &[u8], type_: Type) -> Descriptor {
    let mut buf = [0u8; 56];
    buf[..name.len()].copy_from_slice(name);
    Descriptor::new(buf, type_ as u64)
}

#[test]
fn vmo_descriptors_validation() {
    let entries = [make_descriptor(b"alpha", Type::Sum)];
    let table_bytes = entries.as_bytes().len() as u64;
    let valid_header = DescriptorVmo {
        magic: DescriptorVmo::MAGIC,
        max_cpus: 2,
        descriptor_table_size: table_bytes,
    };
    let vmo = make_desc_vmo(&valid_header, &entries);

    // Header truncated.
    assert_eq!(
        Descriptors::from_vmo(&vmo, (size_of::<DescriptorVmo>() - 1) as u64).unwrap_err(),
        DescriptorsError::TooSmall {
            size: (size_of::<DescriptorVmo>() - 1) as u64,
            descriptor_table_size: 0,
        }
    );

    // Table truncated.
    let short_size = size_of::<DescriptorVmo>() as u64 + table_bytes - 1;
    assert_eq!(
        Descriptors::from_vmo(&vmo, short_size).unwrap_err(),
        DescriptorsError::TooSmall { size: short_size, descriptor_table_size: table_bytes }
    );

    // Bad magic.
    let bad_magic_vmo = make_desc_vmo(&DescriptorVmo { magic: 42, ..valid_header }, &entries);
    assert_eq!(
        Descriptors::from_vmo(&bad_magic_vmo, 4096).unwrap_err(),
        DescriptorsError::BadMagic { magic: 42 }
    );

    // Overflowing `descriptor_table_size`.
    let overflow_vmo =
        make_desc_vmo(&DescriptorVmo { descriptor_table_size: u64::MAX, ..valid_header }, &entries);
    assert_eq!(
        Descriptors::from_vmo(&overflow_vmo, 4096).unwrap_err(),
        DescriptorsError::TooSmall { size: 4096, descriptor_table_size: u64::MAX }
    );

    // Overflowing `arena_size()`.
    let huge_cpus_vmo =
        make_desc_vmo(&DescriptorVmo { max_cpus: u64::MAX, ..valid_header }, &entries);
    let huge_cpus_desc = Descriptors::from_vmo(&huge_cpus_vmo, 4096).expect("valid descriptors");
    assert_eq!(huge_cpus_desc.arena_size(), None);
    let arena_vmo = zx::Vmo::create(4096).expect("create arena VMO");
    // SAFETY: `arena_vmo` is local to this test and never mutated, and nothing
    // modifies the resulting mapping in `vmar_root_self()`.
    assert_eq!(
        unsafe { Arena::map(&arena_vmo, 4096, &huge_cpus_desc) }.unwrap_err(),
        ArenaError::TooSmall
    );
}

#[test]
fn vmo_synthetic_arena_reads() {
    let full_56 = [b'x'; 56];
    let entries = [make_descriptor(b"a.sum", Type::Sum), make_descriptor(&full_56, Type::Min)];
    let header = DescriptorVmo {
        magic: DescriptorVmo::MAGIC,
        max_cpus: 2,
        descriptor_table_size: entries.as_bytes().len() as u64,
    };
    let desc_vmo = make_desc_vmo(&header, &entries);
    let desc = Descriptors::from_vmo(&desc_vmo, 4096).expect("valid descriptors");
    assert_eq!(desc.num_counters(), 2);
    assert_eq!(desc.table()[0].name_bytes(), b"a.sum");
    assert_eq!(desc.table()[1].name_bytes(), &full_56);

    let values: [i64; 4] = [10, -1, -3, 99];
    let arena_vmo = zx::Vmo::create(4096).expect("create arena VMO");
    arena_vmo.write(values.as_bytes(), 0).expect("write arena values");

    // Undersized `size` argument is rejected before mapping.
    // SAFETY: `arena_vmo` is not mutated after initialization above, and nothing
    // modifies the resulting mapping in `vmar_root_self()`.
    assert_eq!(
        unsafe { Arena::map(&arena_vmo, desc.arena_size().unwrap() - 1, &desc) }.unwrap_err(),
        ArenaError::TooSmall
    );

    // Non-page-aligned `size == desc.arena_size()` succeeds and unmaps cleanly on drop.
    // SAFETY: `arena_vmo` is not mutated after initialization above, and nothing
    // modifies the resulting mapping in `vmar_root_self()`.
    let exact_arena =
        unsafe { Arena::map(&arena_vmo, desc.arena_size().unwrap(), &desc) }.expect("map exact");
    assert_eq!(exact_arena.get(0, 0), 10);
    assert_eq!(exact_arena.get(1, 1), 99);
    drop(exact_arena);

    // SAFETY: `arena_vmo` is not mutated after initialization above, and nothing
    // modifies the resulting mapping in `vmar_root_self()`.
    let arena = unsafe { Arena::map(&arena_vmo, 4096, &desc) }.expect("map synthetic arena");
    assert_eq!(arena.get(0, 0), 10);
    assert_eq!(arena.get(0, 1), -1);
    assert_eq!(arena.get(1, 0), -3);
    assert_eq!(arena.get(1, 1), 99);
}

#[test]
fn vmo_arena_vmo_info_validation_and_empty_arena() {
    let entries = [make_descriptor(b"c", Type::Sum)];
    let header = DescriptorVmo {
        magic: DescriptorVmo::MAGIC,
        max_cpus: 2,
        descriptor_table_size: entries.as_bytes().len() as u64,
    };
    let desc_vmo = make_desc_vmo(&header, &entries);
    let desc = Descriptors::from_vmo(&desc_vmo, 4096).expect("valid descriptors");

    // Overstated `size` on a 0-byte VMO is caught via `vmo.info()`.
    let empty_vmo = zx::Vmo::create(0).expect("create 0-byte VMO");
    // SAFETY: `empty_vmo` is local to this test and never mutated, and nothing
    // modifies the resulting mapping in `vmar_root_self()`.
    assert_eq!(unsafe { Arena::map(&empty_vmo, 8192, &desc) }.unwrap_err(), ArenaError::TooSmall);

    // Resizable VMOs are rejected.
    let resizable_vmo =
        zx::Vmo::create_with_opts(zx::VmoOptions::RESIZABLE, 4096).expect("create resizable VMO");
    // SAFETY: `resizable_vmo` is local to this test and never mutated, and
    // nothing modifies the resulting mapping in `vmar_root_self()`.
    assert_eq!(
        unsafe { Arena::map(&resizable_vmo, 4096, &desc) }.unwrap_err(),
        ArenaError::Map(zx::Status::NOT_SUPPORTED)
    );

    // User-pager-backed VMOs are rejected.
    let pager = zx::Pager::create(zx::PagerOptions::empty()).expect("create pager");
    let port = zx::Port::create();
    let pager_vmo =
        pager.create_vmo(zx::VmoOptions::empty(), &port, 0, 4096).expect("create pager-backed VMO");
    // SAFETY: `pager_vmo` is local to this test and never mutated, and nothing
    // modifies the resulting mapping in `vmar_root_self()`.
    assert_eq!(
        unsafe { Arena::map(&pager_vmo, 4096, &desc) }.unwrap_err(),
        ArenaError::Map(zx::Status::NOT_SUPPORTED)
    );

    // Non-cached VMOs are rejected.
    let uncached_vmo = zx::Vmo::create(4096).expect("create uncached VMO");
    uncached_vmo.set_cache_policy(zx::CachePolicy::UnCached).expect("set UnCached policy");
    // SAFETY: `uncached_vmo` is local to this test and not modified after
    // `set_cache_policy` above, and nothing modifies the resulting mapping in
    // `vmar_root_self()`.
    assert_eq!(
        unsafe { Arena::map(&uncached_vmo, 4096, &desc) }.unwrap_err(),
        ArenaError::Map(zx::Status::NOT_SUPPORTED)
    );

    // Zero-counter (`needed == 0`) arena skips `zx_vmar_map` and drops cleanly.
    let empty_desc_vmo = make_desc_vmo(&DescriptorVmo { descriptor_table_size: 0, ..header }, &[]);
    let empty_desc = Descriptors::from_vmo(&empty_desc_vmo, 4096).expect("empty descriptors");
    assert_eq!(empty_desc.arena_size(), Some(0));
    // SAFETY: `empty_vmo` is local to this test and never mutated, and nothing
    // modifies the resulting mapping in `vmar_root_self()`.
    let empty_arena = unsafe { Arena::map(&empty_vmo, 0, &empty_desc) }.expect("map empty arena");
    drop(empty_arena);
}

#[test]
#[should_panic(expected = "CPU 2 out of range")]
fn vmo_arena_cpu_out_of_bounds() {
    let entries = [make_descriptor(b"c", Type::Sum)];
    let header = DescriptorVmo {
        magic: DescriptorVmo::MAGIC,
        max_cpus: 2,
        descriptor_table_size: entries.as_bytes().len() as u64,
    };
    let desc_vmo = make_desc_vmo(&header, &entries);
    let desc = Descriptors::from_vmo(&desc_vmo, 4096).unwrap();
    let arena_vmo = zx::Vmo::create(4096).unwrap();
    // SAFETY: `arena_vmo` is local to this test and never mutated, and nothing
    // modifies the resulting mapping in `vmar_root_self()`.
    let arena = unsafe { Arena::map(&arena_vmo, 4096, &desc) }.unwrap();
    let _ = arena.get(2, 0);
}

#[test]
#[should_panic(expected = "counter index 1 out of range")]
fn vmo_arena_index_out_of_bounds() {
    let entries = [make_descriptor(b"c", Type::Sum)];
    let header = DescriptorVmo {
        magic: DescriptorVmo::MAGIC,
        max_cpus: 2,
        descriptor_table_size: entries.as_bytes().len() as u64,
    };
    let desc_vmo = make_desc_vmo(&header, &entries);
    let desc = Descriptors::from_vmo(&desc_vmo, 4096).unwrap();
    let arena_vmo = zx::Vmo::create(4096).unwrap();
    // SAFETY: `arena_vmo` is local to this test and never mutated, and nothing
    // modifies the resulting mapping in `vmar_root_self()`.
    let arena = unsafe { Arena::map(&arena_vmo, 4096, &desc) }.unwrap();
    let _ = arena.get(0, 1);
}

/// Parses `argv`, returning the result and the error output.
fn parse_with_err(argv: &[&str]) -> (Option<KcounterCmdline>, String) {
    let mut argv = argv.to_vec();
    let mut err = Vec::new();
    let result = kcounter_parse_cmdline(&mut argv, Some(&mut err));
    (result, String::from_utf8(err).expect("error output is UTF-8"))
}

#[test]
fn cmdline_normal_success() {
    let mut argv = ["self.exe", "-v", "-w", "channel"];

    let cmdline = kcounter_parse_cmdline(&mut argv, /*err=*/ None).expect("parse succeeds");
    assert!(!cmdline.help);
    assert!(!cmdline.list);
    assert!(!cmdline.terse);
    assert!(cmdline.verbose);
    assert!(cmdline.period > 1);
    assert_eq!(cmdline.unparsed_args_start, 3);
}

#[test]
fn cmdline_fail_list_and_terse() {
    let (result, err) = parse_with_err(&["self.exe", "-l", "-t"]);
    assert_eq!(result, None);
    assert!(err.contains("--list, --terse"), "{err}");
    assert!(err.contains("Usage: self.exe"), "{err}");
}

#[test]
fn cmdline_fail_terse_and_verbose() {
    let (result, err) = parse_with_err(&["self.exe", "--terse", "-v"]);
    assert_eq!(result, None);
    assert!(err.contains("--terse, and --verbose are mutually exclusive"), "{err}");
}

#[test]
fn cmdline_fail_list_and_watch() {
    let (result, err) = parse_with_err(&["self.exe", "-l", "-w", "things"]);
    assert_eq!(result, None);
    assert!(err.contains("--list and --watch are mutually exclusive"), "{err}");
}
