//! `prepare` on a PE32+ executable built byte by byte here, so the patching
//! is covered on every host. The unit tests in `src/prepare.rs` use the test
//! binary itself, which is a PE image only on Windows.

use victory_loader::prepare;
use victory_pe::ImageInfo;
use victory_pe::layout::{self, dir, dll_flags, file_flags};

const IMAGE_BASE: u64 = 0x1_4000_0000;
const SECTION_RVA: u32 = 0x1000;
const SECTION_FILE: usize = 0x200;
const SECTION_SIZE: usize = 0x400;

// RVAs inside the one section.
const IMPORTS: u32 = 0x1000; // one descriptor, then the null terminator
const INT: u32 = 0x1040;
const IAT: u32 = 0x1060;
const HINT_NAME: u32 = 0x1080;
const DLL_NAME: u32 = 0x10A0;
const TLS: u32 = 0x1100;
const TLS_INDEX: u32 = 0x1180;
const CALLBACKS: u32 = 0x1140;
const CODE: u32 = 0x1200;
const RELOCS: u32 = 0x1300;

const NT: usize = 0x40;
const FILE_HEADER: usize = NT + 4;
const OPT: usize = FILE_HEADER + 20;
const SECTIONS: usize = OPT + 240;

const CHECKSUM: u32 = 0x0001_2345;
const DLL_CHARACTERISTICS: u16 = dll_flags::HIGH_ENTROPY_VA
    | dll_flags::DYNAMIC_BASE
    | dll_flags::NX_COMPAT
    | dll_flags::FORCE_INTEGRITY;
/// The bound-import directory as an old linker might leave it.
const BOUND: [u8; 8] = [0xF0, 0x01, 0, 0, 0x10, 0, 0, 0];

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    b[at..at + v.len()].copy_from_slice(v);
}
fn put16(b: &mut [u8], at: usize, v: u16) {
    put(b, at, &v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    put(b, at, &v.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    put(b, at, &v.to_le_bytes());
}
fn file(rva: u32) -> usize {
    SECTION_FILE + (rva - SECTION_RVA) as usize
}
fn set_dir(b: &mut [u8], index: usize, rva: u32, size: u32) {
    let at = OPT + layout::OPT_DATA_DIRECTORY + index * layout::DATA_DIRECTORY_SIZE;
    put32(b, at, rva);
    put32(b, at + 4, size);
}

/// An executable with one import module (`ExitProcess` by name, ordinal 5),
/// one TLS callback and DIR64 relocations for the TLS directory's pointers.
fn image() -> Vec<u8> {
    let mut b = vec![0u8; SECTION_FILE + SECTION_SIZE];
    put(&mut b, 0, b"MZ");
    put32(&mut b, layout::DOS_E_LFANEW, NT as u32);
    put(&mut b, NT, b"PE\0\0");

    put16(&mut b, FILE_HEADER, 0x8664); // AMD64
    put16(&mut b, FILE_HEADER + 2, 1); // sections
    put32(&mut b, FILE_HEADER + 4, 0x6000_0000); // timestamp
    put16(&mut b, FILE_HEADER + 16, 240); // optional header size
    put16(&mut b, FILE_HEADER + 18, 0x0022); // executable, large address aware

    put16(&mut b, OPT, 0x20B); // PE32+
    put32(&mut b, OPT + layout::OPT_ADDRESS_OF_ENTRY_POINT, CODE);
    put64(&mut b, OPT + 24, IMAGE_BASE);
    put32(&mut b, OPT + 32, 0x1000); // section alignment
    put32(&mut b, OPT + 36, 0x200); // file alignment
    put16(&mut b, OPT + 40, 6); // OS version
    put16(&mut b, OPT + 48, 6); // subsystem version
    put32(&mut b, OPT + 56, 0x2000); // size of image
    put32(&mut b, OPT + 60, SECTION_FILE as u32); // size of headers
    put32(&mut b, OPT + layout::OPT_CHECKSUM, CHECKSUM);
    put16(&mut b, OPT + 68, layout::subsystem::WINDOWS_CUI);
    put16(
        &mut b,
        OPT + layout::OPT_DLL_CHARACTERISTICS,
        DLL_CHARACTERISTICS,
    );
    put64(&mut b, OPT + 72, 0x10_0000); // stack reserve
    put64(&mut b, OPT + 80, 0x1000); // stack commit
    put32(&mut b, OPT + layout::OPT_NUMBER_OF_RVA_AND_SIZES, 16);
    set_dir(&mut b, dir::IMPORT, IMPORTS, 2 * 20);
    set_dir(&mut b, dir::BASERELOC, RELOCS, 16);
    set_dir(&mut b, 9, TLS, 40); // TLS
    put(
        &mut b,
        OPT + layout::OPT_DATA_DIRECTORY + dir::BOUND_IMPORT * layout::DATA_DIRECTORY_SIZE,
        &BOUND,
    );
    set_dir(&mut b, dir::IAT, IAT, 24);

    put(&mut b, SECTIONS, b".data\0\0\0");
    put32(&mut b, SECTIONS + 8, SECTION_SIZE as u32); // virtual size
    put32(&mut b, SECTIONS + 12, SECTION_RVA);
    put32(&mut b, SECTIONS + 16, SECTION_SIZE as u32); // raw size
    put32(&mut b, SECTIONS + 20, SECTION_FILE as u32);
    put32(&mut b, SECTIONS + 36, 0xE000_0060); // code, data, rwx

    // Import descriptor: name table, DLL name, IAT. The terminator is zeros.
    put32(&mut b, file(IMPORTS), INT);
    put32(&mut b, file(IMPORTS) + 12, DLL_NAME);
    put32(&mut b, file(IMPORTS) + 16, IAT);
    for table in [INT, IAT] {
        put64(&mut b, file(table), u64::from(HINT_NAME));
        put64(&mut b, file(table) + 8, 0x8000_0000_0000_0005);
    }
    put16(&mut b, file(HINT_NAME), 7);
    put(&mut b, file(HINT_NAME) + 2, b"ExitProcess\0");
    put(&mut b, file(DLL_NAME), b"KERNEL32.dll\0");

    // TLS directory: no template, an index slot and one callback.
    put64(&mut b, file(TLS) + 16, IMAGE_BASE + u64::from(TLS_INDEX));
    put64(
        &mut b,
        file(TLS) + layout::TLS64_ADDRESS_OF_CALLBACKS,
        IMAGE_BASE + u64::from(CALLBACKS),
    );
    put64(&mut b, file(CALLBACKS), IMAGE_BASE + u64::from(CODE));
    b[file(CODE)] = 0xC3; // ret

    // One block of base relocations for the page at SECTION_RVA.
    put32(&mut b, file(RELOCS), SECTION_RVA);
    put32(&mut b, file(RELOCS) + 4, 16);
    for (i, field) in [TLS + 16, callbacks_field(), CALLBACKS]
        .into_iter()
        .enumerate()
    {
        put16(&mut b, file(RELOCS) + 8 + i * 2, dir64(field));
    }
    b
}

fn callbacks_field() -> u32 {
    TLS + layout::TLS64_ADDRESS_OF_CALLBACKS as u32
}
fn dir64(rva: u32) -> u16 {
    (10 << 12) | (rva - SECTION_RVA) as u16
}
/// File offset of the relocation entry for the TLS callbacks pointer.
fn callbacks_reloc() -> usize {
    file(RELOCS) + 8 + 2
}

fn dir_offset(index: usize) -> usize {
    OPT + layout::OPT_DATA_DIRECTORY + index * layout::DATA_DIRECTORY_SIZE
}

#[test]
fn the_synthetic_image_parses_as_built() {
    let info = ImageInfo::parse(&image()).unwrap();
    assert!(!info.is_dll());
    assert_eq!(info.entry_rva, CODE);
    assert_eq!(info.import_count(), 2);
    assert_eq!(info.imports[0].dll, "KERNEL32.dll");
    let tls = info.tls.as_ref().unwrap();
    assert_eq!(tls.directory_rva, TLS);
    assert_eq!(tls.callback_rvas, [CODE]);
    assert!(info.has_relocations);
}

#[test]
fn prepare_maps_an_inert_dll_and_keeps_what_the_title_expects() {
    let original = image();
    let prepared = prepare(&original).unwrap();

    // `info` describes the original image.
    assert!(!prepared.info.is_dll());
    assert_eq!(prepared.info.entry_rva, CODE);
    assert_eq!(prepared.info.import_count(), 2);
    let slots: Vec<u32> = prepared.info.imports[0]
        .entries
        .iter()
        .map(|e| e.iat_slot_rva)
        .collect();
    assert_eq!(slots, [IAT, IAT + 8]);
    assert_eq!(prepared.info.tls.as_ref().unwrap().callback_rvas, [CODE]);
    assert_eq!(prepared.original_hash, blake3::hash(&original));

    let patched = ImageInfo::parse(&prepared.bytes).unwrap();
    assert!(patched.is_dll());
    assert_eq!(
        patched.characteristics,
        prepared.info.characteristics | file_flags::DLL
    );
    assert_eq!(patched.entry_rva, 0);
    assert!(patched.imports.is_empty());
    assert_eq!(
        patched.dll_characteristics,
        DLL_CHARACTERISTICS & !dll_flags::FORCE_INTEGRITY
    );
    assert_eq!(
        layout::read_u32(&prepared.bytes, OPT + layout::OPT_CHECKSUM),
        Some(0)
    );
    let tls = patched.tls.as_ref().unwrap();
    assert_eq!(tls.directory_rva, TLS);
    assert_eq!(tls.callbacks_va, 0);
    assert!(tls.callback_rvas.is_empty());

    // The import directory points at the empty terminator.
    let import = dir_offset(dir::IMPORT);
    assert_eq!(
        layout::read_u32(&prepared.bytes, import),
        Some(IMPORTS + 20)
    );
    assert_eq!(layout::read_u32(&prepared.bytes, import + 4), Some(20));
    assert_eq!(&prepared.bytes[file(IMPORTS + 20)..][..20], &[0; 20]);

    // Only the callbacks pointer's relocation became a no-op.
    assert_eq!(&prepared.bytes[callbacks_reloc()..][..2], &[0, 0]);
    assert_ne!(&original[callbacks_reloc()..][..2], &[0, 0]);

    // Nothing else changed: the IAT, its directory, the name tables, the
    // callback array and the other relocations are as the title left them.
    assert_eq!(prepared.bytes.len(), original.len());
    let expected = expected_changes();
    let differing: Vec<usize> = (0..original.len())
        .filter(|&i| original[i] != prepared.bytes[i])
        .collect();
    assert!(
        differing
            .iter()
            .all(|i| expected.iter().any(|r| r.contains(i))),
        "unexpected bytes changed: {:#x?}",
        differing
            .iter()
            .filter(|i| !expected.iter().any(|r| r.contains(i)))
            .collect::<Vec<_>>()
    );
}

/// Byte ranges `prepare` may write.
fn expected_changes() -> Vec<std::ops::Range<usize>> {
    let field = |at: usize, len: usize| at..at + len;
    vec![
        field(FILE_HEADER + layout::FILE_HEADER_CHARACTERISTICS, 2),
        field(OPT + layout::OPT_ADDRESS_OF_ENTRY_POINT, 4),
        field(OPT + layout::OPT_CHECKSUM, 4),
        field(OPT + layout::OPT_DLL_CHARACTERISTICS, 2),
        field(dir_offset(dir::IMPORT), layout::DATA_DIRECTORY_SIZE),
        field(dir_offset(dir::BOUND_IMPORT), layout::DATA_DIRECTORY_SIZE),
        field(file(callbacks_field()), 8),
        field(callbacks_reloc(), 2),
    ]
}

#[test]
fn prepare_clears_the_bound_import_directory() {
    let original = image();
    let at = dir_offset(dir::BOUND_IMPORT);
    assert_eq!(&original[at..][..8], &BOUND);
    let prepared = prepare(&original).unwrap();
    assert_eq!(&prepared.bytes[at..][..8], &[0; 8]);
}

#[test]
fn import_size_that_excludes_the_terminator_still_finds_it() {
    // Some linkers count the descriptors for real modules only.
    let mut original = image();
    put32(&mut original, dir_offset(dir::IMPORT) + 4, 20);
    let prepared = prepare(&original).unwrap();
    assert_eq!(prepared.info.import_count(), 2);
    let import = dir_offset(dir::IMPORT);
    assert_eq!(
        layout::read_u32(&prepared.bytes, import),
        Some(IMPORTS + 20)
    );
    assert_eq!(layout::read_u32(&prepared.bytes, import + 4), Some(20));
    assert!(
        ImageInfo::parse(&prepared.bytes)
            .unwrap()
            .imports
            .is_empty()
    );
}

#[test]
fn the_terminator_follows_every_import_descriptor() {
    // Three descriptors, the third for the same DLL as the first in another
    // case, as linkers emit when objects spell it differently. Each one is
    // its own module, so the terminator sits 3 * 20 bytes in.
    const TABLE: u32 = 0x1380; // 4 * 20 bytes, up to the section end
    const IAT2: u32 = 0x1210;
    const IAT3: u32 = 0x1220;
    const NAME2: u32 = 0x1230;
    const NAME3: u32 = 0x1240;
    let mut original = image();
    let old = file(IMPORTS);
    let descriptor = original[old..old + 20].to_vec();
    put(&mut original, file(TABLE), &descriptor);
    for (i, (iat, name, thunk)) in [
        (IAT2, NAME2, u64::from(HINT_NAME)),
        (IAT3, NAME3, 0x8000_0000_0000_0001),
    ]
    .into_iter()
    .enumerate()
    {
        // No name table: old linkers leave OriginalFirstThunk at 0.
        let at = file(TABLE) + (i + 1) * 20;
        put32(&mut original, at + 12, name);
        put32(&mut original, at + 16, iat);
        put64(&mut original, file(iat), thunk);
    }
    put(&mut original, file(NAME2), b"USER32.dll\0");
    put(&mut original, file(NAME3), b"kernel32.dll\0");
    set_dir(&mut original, dir::IMPORT, TABLE, 4 * 20);

    let prepared = prepare(&original).unwrap();
    let modules: Vec<(&str, Vec<u32>)> = prepared
        .info
        .imports
        .iter()
        .map(|m| {
            let slots = m.entries.iter().map(|e| e.iat_slot_rva).collect();
            (m.dll.as_str(), slots)
        })
        .collect();
    assert_eq!(
        modules,
        [
            ("KERNEL32.dll", vec![IAT, IAT + 8]),
            ("USER32.dll", vec![IAT2]),
            ("kernel32.dll", vec![IAT3]),
        ]
    );

    let import = dir_offset(dir::IMPORT);
    assert_eq!(
        layout::read_u32(&prepared.bytes, import),
        Some(TABLE + 3 * 20)
    );
    assert_eq!(layout::read_u32(&prepared.bytes, import + 4), Some(20));
    assert_eq!(&prepared.bytes[file(TABLE + 3 * 20)..][..20], &[0; 20]);
    assert!(
        ImageInfo::parse(&prepared.bytes)
            .unwrap()
            .imports
            .is_empty()
    );
    // The descriptors and the IATs the title binds against are untouched.
    let differing: Vec<usize> = (0..original.len())
        .filter(|&i| original[i] != prepared.bytes[i])
        .collect();
    let expected = expected_changes();
    assert!(
        differing
            .iter()
            .all(|i| expected.iter().any(|r| r.contains(i))),
        "unexpected bytes changed: {differing:#x?}"
    );
}

#[test]
fn an_image_without_imports_or_tls_only_changes_its_headers() {
    let mut original = image();
    set_dir(&mut original, dir::IMPORT, 0, 0);
    set_dir(&mut original, dir::IAT, 0, 0);
    set_dir(&mut original, 9, 0, 0);
    let prepared = prepare(&original).unwrap();
    assert!(prepared.info.imports.is_empty());
    assert!(prepared.info.tls.is_none());
    let import = dir_offset(dir::IMPORT);
    assert_eq!(&prepared.bytes[import..][..8], &[0; 8]);
    // The section, relocations included, is untouched.
    assert_eq!(&prepared.bytes[SECTION_FILE..], &original[SECTION_FILE..]);
    assert!(ImageInfo::parse(&prepared.bytes).unwrap().is_dll());
}

#[test]
fn file_name_names_the_original_build() {
    let original = image();
    let a = prepare(&original).unwrap();
    let b = prepare(&original).unwrap();
    assert_eq!(a.file_name(), b.file_name());
    let hex = &blake3::hash(&original).to_hex()[..16];
    assert_eq!(a.file_name(), format!("victory-image-{hex}.dll"));

    let mut other = original;
    put32(&mut other, FILE_HEADER + 4, 0x6000_0001); // another timestamp
    assert_ne!(prepare(&other).unwrap().file_name(), a.file_name());
}

#[test]
fn what_is_not_a_pe32_plus_image_is_an_error() {
    assert!(prepare(&[]).is_err());
    assert!(prepare(b"MZ").is_err());
    let original = image();
    for len in [0x40, 0x100, SECTIONS + 20] {
        assert!(prepare(&original[..len]).is_err(), "truncated to {len:#x}");
    }
    let mut x86 = original;
    put16(&mut x86, FILE_HEADER, 0x014C);
    assert!(prepare(&x86).is_err());
}

/// Applies every DIR64 base relocation in `bytes` for a load `delta` bytes
/// above the preferred base, as the Windows or Wine loader does on a rebase.
fn rebase(bytes: &mut [u8], delta: u64) {
    let info = ImageInfo::parse(bytes).unwrap();
    let d = dir_offset(dir::BASERELOC);
    let (rva, size) = (
        layout::read_u32(bytes, d).unwrap(),
        layout::read_u32(bytes, d + 4).unwrap(),
    );
    if rva == 0 || size == 0 {
        return;
    }
    let start = info.rva_to_file_offset(rva).unwrap() as usize;
    let end = start + size as usize;
    let mut at = start;
    while at + 8 <= end {
        let page = layout::read_u32(bytes, at).unwrap();
        let block = layout::read_u32(bytes, at + 4).unwrap() as usize;
        assert!(block >= 8 && at + block <= end, "well-formed blocks only");
        for entry in (at + 8..at + block).step_by(2) {
            let e = u16::from_le_bytes([bytes[entry], bytes[entry + 1]]);
            if e >> 12 == 10 {
                let target = info
                    .rva_to_file_offset(page + u32::from(e & 0xFFF))
                    .unwrap() as usize;
                let v = layout::read_u64(bytes, target).unwrap();
                put64(bytes, target, v.wrapping_add(delta));
            }
        }
        at += block;
    }
}

#[test]
fn a_rebase_keeps_the_cleared_callback_pointer_at_zero() {
    // Wine maps DYNAMIC_BASE images away from their preferred base, so the
    // loader applies every relocation before it looks at the TLS directory.
    let delta = 0x7_0000_0000;
    let original = image();
    let mut mapped = prepare(&original).unwrap().bytes;
    rebase(&mut mapped, delta);
    let tls = file(TLS);
    assert_eq!(
        layout::read_u64(&mapped, tls + layout::TLS64_ADDRESS_OF_CALLBACKS),
        Some(0),
        "a relocated 0 would point the loader at callbacks below the image"
    );
    // The other pointers still move with the image.
    assert_eq!(
        layout::read_u64(&mapped, tls + 16),
        Some(IMAGE_BASE + u64::from(TLS_INDEX) + delta)
    );
    assert_eq!(
        layout::read_u64(&mapped, file(CALLBACKS)),
        Some(IMAGE_BASE + u64::from(CODE) + delta)
    );

    // Without prepare's no-op, the same rebase would leave `delta` there.
    let mut unpatched = original;
    put64(
        &mut unpatched,
        file(callbacks_field()),
        0, // what prepare writes, relocation left alone
    );
    rebase(&mut unpatched, delta);
    assert_eq!(
        layout::read_u64(&unpatched, file(callbacks_field())),
        Some(delta)
    );
}

#[test]
fn the_callback_relocation_is_found_in_a_later_block() {
    // Two blocks: the first covers the import page with a HIGHLOW entry for
    // the same RVA (not a 64-bit pointer, so not ours to touch), the second
    // holds the DIR64 entry for the callbacks pointer.
    let mut original = image();
    let first = file(RELOCS);
    put32(&mut original, first, SECTION_RVA);
    put32(&mut original, first + 4, 12);
    put16(
        &mut original,
        first + 8,
        (3 << 12) | (callbacks_field() - SECTION_RVA) as u16,
    );
    put16(&mut original, first + 10, dir64(TLS + 16));
    let second = first + 12;
    put32(&mut original, second, SECTION_RVA);
    put32(&mut original, second + 4, 12);
    put16(&mut original, second + 8, dir64(callbacks_field()));
    put16(&mut original, second + 10, 0); // ABSOLUTE padding
    set_dir(&mut original, dir::BASERELOC, RELOCS, 24);

    let prepared = prepare(&original).unwrap();
    assert_eq!(&prepared.bytes[second + 8..][..2], &[0, 0]);
    assert_eq!(
        &prepared.bytes[first + 8..first + 12],
        &original[first + 8..first + 12],
        "the HIGHLOW entry and the index pointer's entry are left alone"
    );
}

#[test]
fn damaged_or_missing_relocations_still_clear_the_callback_pointer() {
    type Damage = (&'static str, fn(&mut Vec<u8>));
    let cases: [Damage; 5] = [
        ("no relocation directory", |b| {
            set_dir(b, dir::BASERELOC, 0, 0)
        }),
        ("relocation directory of size 0", |b| {
            set_dir(b, dir::BASERELOC, RELOCS, 0)
        }),
        ("relocation directory outside the file", |b| {
            set_dir(b, dir::BASERELOC, 0x8000, 16)
        }),
        ("block shorter than its header", |b| {
            put32(b, file(RELOCS) + 4, 4)
        }),
        ("block longer than the directory", |b| {
            put32(b, file(RELOCS) + 4, 0x1000)
        }),
    ];
    for (what, damage) in cases {
        let mut original = image();
        damage(&mut original);
        let prepared = prepare(&original).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(
            layout::read_u64(&prepared.bytes, file(callbacks_field())),
            Some(0),
            "{what}"
        );
        // No relocation entry was rewritten from a block that was not read.
        assert_eq!(
            &prepared.bytes[file(RELOCS)..file(RELOCS) + 16],
            &original[file(RELOCS)..file(RELOCS) + 16],
            "{what}"
        );
    }
}

#[test]
fn tls_without_callbacks_leaves_its_relocations_alone() {
    let mut original = image();
    put64(&mut original, file(callbacks_field()), 0);
    let prepared = prepare(&original).unwrap();
    assert!(prepared.info.tls.as_ref().unwrap().callback_rvas.is_empty());
    assert_eq!(
        &prepared.bytes[file(RELOCS)..file(RELOCS) + 16],
        &original[file(RELOCS)..file(RELOCS) + 16]
    );
}

#[test]
fn preparing_a_prepared_image_changes_nothing() {
    let once = prepare(&image()).unwrap();
    let twice = prepare(&once.bytes).unwrap();
    assert_eq!(twice.bytes, once.bytes);
    assert!(twice.info.is_dll());
    assert!(twice.info.imports.is_empty());
    assert_ne!(twice.file_name(), once.file_name());
}

#[test]
fn a_title_dll_stays_a_dll_with_its_other_flags() {
    let mut original = image();
    let flags = FILE_HEADER + layout::FILE_HEADER_CHARACTERISTICS;
    put16(&mut original, flags, 0x0022 | file_flags::DLL);
    let prepared = prepare(&original).unwrap();
    assert!(prepared.info.is_dll());
    assert_eq!(
        ImageInfo::parse(&prepared.bytes).unwrap().characteristics,
        0x0022 | file_flags::DLL
    );
}

#[test]
fn directory_slots_past_number_of_rva_and_sizes_are_not_written() {
    // A shorter optional header with 10 directories (through TLS): the
    // section table follows directly, so the slot where the bound-import
    // directory would be (index 11) holds the section's virtual size and
    // RVA. Writing it would unmap the section the title runs from.
    const COUNT: usize = 10;
    let opt_size = layout::OPT_DATA_DIRECTORY + COUNT * layout::DATA_DIRECTORY_SIZE;
    let mut original = image();
    put16(&mut original, FILE_HEADER + 16, opt_size as u16);
    put32(
        &mut original,
        OPT + layout::OPT_NUMBER_OF_RVA_AND_SIZES,
        COUNT as u32,
    );
    let section = original[SECTIONS..SECTIONS + 40].to_vec();
    put(&mut original, SECTIONS, &[0; 40]);
    put(&mut original, OPT + opt_size, &section);
    assert!((OPT + opt_size..OPT + opt_size + 40).contains(&dir_offset(dir::BOUND_IMPORT)));

    let info = ImageInfo::parse(&original).unwrap();
    assert_eq!(info.sections[0].rva, SECTION_RVA);
    let prepared = prepare(&original).unwrap();
    let section_table = OPT + opt_size..OPT + opt_size + 40;
    assert_eq!(
        &prepared.bytes[section_table.clone()],
        &original[section_table]
    );
    let patched = ImageInfo::parse(&prepared.bytes).unwrap();
    assert!(patched.is_dll());
    assert!(patched.imports.is_empty());
    assert_eq!(patched.sections[0].rva, SECTION_RVA);
    assert_eq!(patched.sections[0].virtual_size, SECTION_SIZE as u32);
    // The rest of the patch still happened.
    assert_eq!(patched.tls.as_ref().unwrap().callbacks_va, 0);
    assert_eq!(&prepared.bytes[callbacks_reloc()..][..2], &[0, 0]);
}

#[test]
fn every_relocation_of_the_callback_pointer_becomes_a_no_op() {
    // A linker that merges relocation tables can leave two DIR64 entries for
    // one field. Either one alone would turn the cleared pointer into
    // `delta` on a rebase, so both are neutralised.
    let mut original = image();
    let second = file(RELOCS) + 16;
    put32(&mut original, second, SECTION_RVA);
    put32(&mut original, second + 4, 12);
    put16(&mut original, second + 8, dir64(callbacks_field()));
    put16(&mut original, second + 10, 0); // ABSOLUTE padding
    set_dir(&mut original, dir::BASERELOC, RELOCS, 28);

    let prepared = prepare(&original).unwrap();
    assert_eq!(&prepared.bytes[callbacks_reloc()..][..2], &[0, 0]);
    assert_eq!(&prepared.bytes[second + 8..][..2], &[0, 0]);
    let mut mapped = prepared.bytes;
    let delta = 0x2_0000_0000;
    rebase(&mut mapped, delta);
    assert_eq!(layout::read_u64(&mapped, file(callbacks_field())), Some(0));
    assert_eq!(
        layout::read_u64(&mapped, file(TLS) + 16),
        Some(IMAGE_BASE + u64::from(TLS_INDEX) + delta)
    );
}

#[test]
fn an_import_directory_of_only_a_terminator_points_at_it() {
    // A title that imports nothing through the ordinary table but keeps an
    // import directory (its delay imports or IAT are elsewhere): the empty
    // table is the directory itself, given the size of one descriptor
    // whatever size the linker recorded.
    let mut original = image();
    put(&mut original, file(IMPORTS), &[0; 20]);
    set_dir(&mut original, dir::IMPORT, IMPORTS, 2 * 20);
    let prepared = prepare(&original).unwrap();
    assert!(prepared.info.imports.is_empty());
    let import = dir_offset(dir::IMPORT);
    assert_eq!(layout::read_u32(&prepared.bytes, import), Some(IMPORTS));
    assert_eq!(layout::read_u32(&prepared.bytes, import + 4), Some(20));
    assert!(
        ImageInfo::parse(&prepared.bytes)
            .unwrap()
            .imports
            .is_empty()
    );
}

#[test]
fn damaged_images_return_a_result_without_panicking() {
    // Every truncation, and every byte of the headers, directories and
    // tables set to 0x00 and to 0xFF in turn. A patched copy, when there is
    // one, keeps the original's length: `prepare` only overwrites fields.
    let original = image();
    let check = |bytes: &[u8], what: &str| {
        if let Ok(prepared) = prepare(bytes) {
            assert_eq!(prepared.bytes.len(), bytes.len(), "{what}");
            assert_eq!(prepared.original_hash, blake3::hash(bytes), "{what}");
        }
    };
    for len in 0..original.len() {
        check(&original[..len], &format!("truncated to {len:#x}"));
    }
    for at in 0..original.len() {
        for value in [0x00, 0xFF] {
            let mut damaged = original.clone();
            damaged[at] = value;
            check(&damaged, &format!("byte {at:#x} set to {value:#04x}"));
        }
    }
}

#[test]
fn relocations_in_their_own_section_are_read_through_the_section_table() {
    // Linkers put base relocations in a `.reloc` section of their own, at a
    // file offset that is not the `.data` section's. A stale copy of the old
    // block stays in `.data`: the directory no longer points there, so it is
    // left alone. The first `.reloc` block covers another page, with a DIR64
    // entry whose page offset equals the callbacks pointer's: its target is
    // a different RVA, so it is not the pointer's relocation.
    const RELOC_RVA: u32 = 0x2000;
    const RELOC_FILE: usize = SECTION_FILE + SECTION_SIZE;
    const RELOC_SIZE: usize = 0x200;
    let offset = (callbacks_field() - SECTION_RVA) as u16;
    let mut original = image();
    original.resize(RELOC_FILE + RELOC_SIZE, 0);
    put16(&mut original, FILE_HEADER + 2, 2); // sections
    put32(&mut original, OPT + 56, RELOC_RVA + 0x1000); // size of image
    let sec = SECTIONS + 40;
    put(&mut original, sec, b".reloc\0\0");
    put32(&mut original, sec + 8, RELOC_SIZE as u32);
    put32(&mut original, sec + 12, RELOC_RVA);
    put32(&mut original, sec + 16, RELOC_SIZE as u32);
    put32(&mut original, sec + 20, RELOC_FILE as u32);
    put32(&mut original, sec + 36, 0x4200_0040); // data, discardable, read

    let other = RELOC_FILE;
    put32(&mut original, other, RELOC_RVA);
    put32(&mut original, other + 4, 12);
    put16(&mut original, other + 8, (10 << 12) | offset);
    put16(&mut original, other + 10, 0); // ABSOLUTE padding
    let ours = other + 12;
    let block = original[file(RELOCS)..file(RELOCS) + 16].to_vec();
    put(&mut original, ours, &block);
    set_dir(&mut original, dir::BASERELOC, RELOC_RVA, 12 + 16);

    let info = ImageInfo::parse(&original).unwrap();
    assert_eq!(info.sections.len(), 2);
    assert_eq!(info.rva_to_file_offset(RELOC_RVA), Some(RELOC_FILE as u32));

    let prepared = prepare(&original).unwrap();
    assert_eq!(
        layout::read_u64(&prepared.bytes, file(callbacks_field())),
        Some(0)
    );
    // Only the entry in the block for the callbacks pointer's page changes.
    assert_eq!(&prepared.bytes[ours + 8 + 2..][..2], &[0, 0]);
    assert_eq!(
        &prepared.bytes[other..other + 12],
        &original[other..other + 12],
        "same page offset, different page"
    );
    let ours_rest = [ours..ours + 10, ours + 12..ours + 16];
    for range in ours_rest {
        assert_eq!(&prepared.bytes[range.clone()], &original[range]);
    }
    assert_eq!(
        &prepared.bytes[file(RELOCS)..file(RELOCS) + 16],
        &original[file(RELOCS)..file(RELOCS) + 16],
        "the stale block in `.data` is not a relocation table any more"
    );

    let delta = 0x3_0000_0000;
    let mut mapped = prepared.bytes;
    rebase(&mut mapped, delta);
    assert_eq!(layout::read_u64(&mapped, file(callbacks_field())), Some(0));
    assert_eq!(
        layout::read_u64(&mapped, file(TLS) + 16),
        Some(IMAGE_BASE + u64::from(TLS_INDEX) + delta)
    );
    assert_eq!(
        layout::read_u64(&mapped, file(CALLBACKS)),
        Some(IMAGE_BASE + u64::from(CODE) + delta)
    );
}

#[test]
fn an_import_directory_without_an_rva_is_left_as_it_is() {
    // A size with no RVA is no import table. There is no terminator to point
    // at, so the slot is kept rather than aimed at the headers.
    let mut original = image();
    set_dir(&mut original, dir::IMPORT, 0, 40);
    set_dir(&mut original, dir::IAT, 0, 0);
    let prepared = prepare(&original).unwrap();
    assert!(prepared.info.imports.is_empty());
    let import = dir_offset(dir::IMPORT);
    assert_eq!(
        &prepared.bytes[import..import + 8],
        &original[import..import + 8]
    );
    // The rest of the patch still happened.
    let patched = ImageInfo::parse(&prepared.bytes).unwrap();
    assert!(patched.is_dll());
    assert_eq!(patched.entry_rva, 0);
    assert_eq!(patched.tls.as_ref().unwrap().callbacks_va, 0);
    assert_eq!(
        &prepared.bytes[dir_offset(dir::BOUND_IMPORT)..][..8],
        &[0; 8]
    );
}
