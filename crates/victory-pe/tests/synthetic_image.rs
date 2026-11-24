//! `ImageInfo` against a small PE32+ image built in the test, so these checks
//! run on every host. The unit tests that parse the test binary itself only
//! run where that binary is a PE image (Windows).

use victory_pe::layout::{self, dir};
use victory_pe::{ImageInfo, ImportSymbol, MACHINE_AMD64, PeError};

const IMAGE_BASE: u64 = 0x1_4000_0000;
/// The one section: RVA 0x1000, file offset 0x200, 0x400 bytes.
const SECTION_RVA: u32 = 0x1000;
const SECTION_FILE_OFFSET: u32 = 0x200;
const SECTION_SIZE: u32 = 0x400;
const SIZE_OF_IMAGE: u32 = 0x2000;
const OPT: usize = 0x58;
const DIR_TLS: usize = 9;

/// Section contents, by RVA.
const ENTRY: u32 = 0x1000;
const IMPORT_DESCRIPTORS: u32 = 0x1100;
const NAME_TABLE: u32 = 0x1180;
const IAT: u32 = 0x11A0;
const DLL_NAME: u32 = 0x11C0;
const HINT_NAME: u32 = 0x11D0;
const TLS_DIRECTORY: u32 = 0x1200;
const TLS_CALLBACKS: u32 = 0x1240;

fn put16(d: &mut [u8], at: usize, v: u16) {
    d[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(d: &mut [u8], at: usize, v: u32) {
    d[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(d: &mut [u8], at: usize, v: u64) {
    d[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// File offset of an RVA inside the section.
fn at(rva: u32) -> usize {
    (rva - SECTION_RVA + SECTION_FILE_OFFSET) as usize
}

/// An x86-64 executable importing `KERNEL32.dll!ExitProcess` (hint 0x123)
/// and ordinal 5 from it, with two TLS callbacks.
fn image() -> Vec<u8> {
    let mut d = vec![0u8; (SECTION_FILE_OFFSET + SECTION_SIZE) as usize];
    d[0..2].copy_from_slice(b"MZ");
    put32(&mut d, layout::DOS_E_LFANEW, 0x40);
    d[0x40..0x44].copy_from_slice(b"PE\0\0");
    put16(&mut d, 0x44, MACHINE_AMD64);
    put16(&mut d, 0x46, 1); // sections
    put32(&mut d, 0x48, 0x6000_0000); // timestamp
    put16(&mut d, 0x54, 240); // optional header size
    put16(&mut d, 0x56, 0x0022); // executable, large address aware

    put16(&mut d, OPT, 0x20B);
    put32(&mut d, OPT + 16, ENTRY);
    put64(&mut d, OPT + 24, IMAGE_BASE);
    put32(&mut d, OPT + 32, 0x1000); // section alignment
    put32(&mut d, OPT + 36, 0x200); // file alignment
    put32(&mut d, OPT + 56, SIZE_OF_IMAGE);
    put32(&mut d, OPT + 60, 0x200); // size of headers
    put16(&mut d, OPT + 68, layout::subsystem::WINDOWS_CUI);
    put64(&mut d, OPT + 72, 0x10_0000); // stack reserve
    put64(&mut d, OPT + 80, 0x1000); // stack commit
    put32(&mut d, OPT + 108, 16);
    let dd = |index: usize| OPT + layout::OPT_DATA_DIRECTORY + index * 8;
    put32(&mut d, dd(dir::IMPORT), IMPORT_DESCRIPTORS);
    put32(&mut d, dd(dir::IMPORT) + 4, 40);
    put32(&mut d, dd(DIR_TLS), TLS_DIRECTORY);
    put32(&mut d, dd(DIR_TLS) + 4, 40);

    let sec = OPT + 240;
    d[sec..sec + 5].copy_from_slice(b".text");
    put32(&mut d, sec + 8, SECTION_SIZE);
    put32(&mut d, sec + 12, SECTION_RVA);
    put32(&mut d, sec + 16, SECTION_SIZE);
    put32(&mut d, sec + 20, SECTION_FILE_OFFSET);
    put32(&mut d, sec + 36, 0xE000_0020); // code, read, write, execute

    d[at(ENTRY)] = 0xC3; // ret

    // One descriptor, then the all-zero terminator.
    let desc = at(IMPORT_DESCRIPTORS);
    put32(&mut d, desc, NAME_TABLE);
    put32(&mut d, desc + 12, DLL_NAME);
    put32(&mut d, desc + 16, IAT);
    for table in [NAME_TABLE, IAT] {
        put64(&mut d, at(table), u64::from(HINT_NAME));
        put64(&mut d, at(table) + 8, (1 << 63) | 5);
    }
    let name = b"KERNEL32.dll\0";
    d[at(DLL_NAME)..at(DLL_NAME) + name.len()].copy_from_slice(name);
    put16(&mut d, at(HINT_NAME), 0x123);
    let func = b"ExitProcess\0";
    d[at(HINT_NAME) + 2..at(HINT_NAME) + 2 + func.len()].copy_from_slice(func);

    put64(
        &mut d,
        at(TLS_DIRECTORY) + layout::TLS64_ADDRESS_OF_CALLBACKS,
        IMAGE_BASE + u64::from(TLS_CALLBACKS),
    );
    put64(&mut d, at(TLS_CALLBACKS), IMAGE_BASE + 0x1010);
    put64(&mut d, at(TLS_CALLBACKS) + 8, IMAGE_BASE + 0x1020);
    d
}

#[test]
fn reads_headers_and_sections() {
    let info = ImageInfo::parse(&image()).unwrap();
    assert_eq!(info.machine, MACHINE_AMD64);
    assert!(!info.is_dll());
    assert_eq!(info.timestamp, 0x6000_0000);
    assert_eq!(info.image_base, IMAGE_BASE);
    assert_eq!(info.size_of_image, SIZE_OF_IMAGE);
    assert_eq!(info.entry_rva, ENTRY);
    assert_eq!(info.subsystem_name(), "windows-cui");
    assert_eq!((info.stack_reserve, info.stack_commit), (0x10_0000, 0x1000));
    assert_eq!(info.sections.len(), 1);
    let s = &info.sections[0];
    assert!(s.name.starts_with(".text"));
    assert_eq!((s.rva, s.virtual_size), (SECTION_RVA, SECTION_SIZE));
    assert_eq!(
        (s.file_offset, s.file_size),
        (SECTION_FILE_OFFSET, SECTION_SIZE)
    );
    assert!(!info.has_relocations);
    assert!(!info.has_exception_table);
    assert!(info.exports.is_empty());
    assert!(info.delay_imports.is_empty());
    assert!(info.codeview.is_none());
}

#[test]
fn reads_imports_with_their_iat_slots() {
    let info = ImageInfo::parse(&image()).unwrap();
    assert_eq!(info.imports.len(), 1);
    let kernel32 = &info.imports[0];
    assert_eq!(kernel32.dll, "KERNEL32.dll");
    let entries: Vec<_> = kernel32
        .entries
        .iter()
        .map(|e| (e.symbol.clone(), e.iat_slot_rva))
        .collect();
    assert_eq!(
        entries,
        [
            (
                ImportSymbol::Name {
                    hint: 0x123,
                    name: "ExitProcess".into()
                },
                IAT
            ),
            (ImportSymbol::Ordinal(5), IAT + 8),
        ]
    );
    assert_eq!(info.import_count(), 2);
}

#[test]
fn reads_tls_callbacks_in_order() {
    let tls = ImageInfo::parse(&image()).unwrap().tls.unwrap();
    assert_eq!(tls.directory_rva, TLS_DIRECTORY);
    assert_eq!(tls.callbacks_va, IMAGE_BASE + u64::from(TLS_CALLBACKS));
    assert_eq!(tls.callback_rvas, [0x1010, 0x1020]);
}

#[test]
fn tls_callback_below_the_image_base_is_malformed() {
    let mut d = image();
    put64(&mut d, at(TLS_CALLBACKS) + 8, IMAGE_BASE - 0x1000);
    assert!(matches!(ImageInfo::parse(&d), Err(PeError::Malformed(_))));
}

#[test]
fn rva_to_file_offset_maps_section_data() {
    let data = image();
    let mut info = ImageInfo::parse(&data).unwrap();
    let entry = info.rva_to_file_offset(info.entry_rva).unwrap() as usize;
    assert_eq!(data[entry], 0xC3);
    assert_eq!(
        info.rva_to_file_offset(HINT_NAME),
        Some(at(HINT_NAME) as u32)
    );
    // Headers and the end of the section are not section data.
    assert_eq!(info.rva_to_file_offset(0x40), None);
    assert_eq!(info.rva_to_file_offset(SECTION_RVA + SECTION_SIZE), None);
    // Raw data past the virtual size is padding, not part of the section.
    info.sections[0].virtual_size = 0x100;
    assert_eq!(info.rva_to_file_offset(SECTION_RVA + 0xFF), Some(0x2FF));
    assert_eq!(info.rva_to_file_offset(SECTION_RVA + 0x100), None);
    // An offset near 4 GiB does not wrap.
    info.sections[0].file_offset = u32::MAX - 0x10;
    assert_eq!(info.rva_to_file_offset(SECTION_RVA + 0x20), None);
}

#[test]
fn rejects_iat_outside_image() {
    for iat in [SIZE_OF_IMAGE - 8, 0xFFFF_FFF8] {
        let mut d = image();
        // The second slot (or the first, at the 4 GiB limit) runs past the end.
        put32(&mut d, at(IMPORT_DESCRIPTORS) + 16, iat);
        assert!(
            matches!(ImageInfo::parse(&d), Err(PeError::Malformed(_))),
            "IAT at {iat:#x}"
        );
    }
}

#[test]
fn rejects_other_machines() {
    let mut d = image();
    put16(&mut d, 0x44, 0xAA64); // ARM64
    assert!(matches!(ImageInfo::parse(&d), Err(PeError::Unsupported(_))));
}

#[test]
fn rejects_truncated_images_without_panicking() {
    let d = image();
    for len in [0, 2, 0x40, 0x58, 0x150, 0x200] {
        assert!(ImageInfo::parse(&d[..len]).is_err(), "{len} bytes");
    }
}

// Optional directories, each added to the image by the test that needs it.
// They live in the free tail of the section (0x1280..0x1400).
const DIR_EXPORT: usize = 0;
const DIR_DEBUG: usize = 6;
const DIR_DELAY_IMPORT: usize = 13;

const DELAY_DESCRIPTORS: u32 = 0x1280;
const DELAY_NAME_TABLE: u32 = 0x12C0;
const DELAY_IAT: u32 = 0x12D0;
const DELAY_DLL_NAME: u32 = 0x12E0;
const DELAY_HINT_NAME: u32 = 0x12F0;
const DEBUG_DIRECTORY: u32 = 0x1300;
const CODEVIEW: u32 = 0x1320;
const EXPORT_DIRECTORY: u32 = 0x1350;
const EXPORT_DIRECTORY_END: u32 = 0x1400;

fn put_bytes(d: &mut [u8], rva: u32, bytes: &[u8]) {
    d[at(rva)..at(rva) + bytes.len()].copy_from_slice(bytes);
}

fn set_directory(d: &mut [u8], index: usize, rva: u32, size: u32) {
    let entry = OPT + layout::OPT_DATA_DIRECTORY + index * layout::DATA_DIRECTORY_SIZE;
    put32(d, entry, rva);
    put32(d, entry + 4, size);
}

/// Delay-loads `USER32.dll!MessageBoxW` (hint 7) with an RVA-based descriptor.
fn add_delay_import(d: &mut [u8]) {
    set_directory(d, DIR_DELAY_IMPORT, DELAY_DESCRIPTORS, 64);
    let desc = at(DELAY_DESCRIPTORS);
    put32(d, desc, 1); // attributes: RVA based
    put32(d, desc + 4, DELAY_DLL_NAME);
    put32(d, desc + 12, DELAY_IAT);
    put32(d, desc + 16, DELAY_NAME_TABLE);
    put64(d, at(DELAY_NAME_TABLE), u64::from(DELAY_HINT_NAME));
    put_bytes(d, DELAY_DLL_NAME, b"USER32.dll\0");
    put16(d, at(DELAY_HINT_NAME), 7);
    put_bytes(d, DELAY_HINT_NAME + 2, b"MessageBoxW\0");
}

/// A CodeView debug directory entry whose record has signature `sig`.
fn add_codeview(d: &mut [u8], sig: &[u8; 4]) {
    set_directory(d, DIR_DEBUG, DEBUG_DIRECTORY, 28);
    let dbg = at(DEBUG_DIRECTORY);
    put32(d, dbg + 12, 2); // IMAGE_DEBUG_TYPE_CODEVIEW
    put32(d, dbg + 16, 4 + 16 + 4 + 6); // size of data
    put32(d, dbg + 20, CODEVIEW);
    put32(d, dbg + 24, at(CODEVIEW) as u32); // pointer to raw data
    put_bytes(d, CODEVIEW, sig);
    let guid: Vec<u8> = (0..16).collect();
    put_bytes(d, CODEVIEW + 4, &guid);
    put32(d, at(CODEVIEW) + 20, 0x2A); // age
    put_bytes(d, CODEVIEW + 24, b"t.pdb\0");
}

/// Exports ordinal 1 `local` at the entry point, ordinal 2 `fwdname` forwarded
/// to `NTDLL.RtlFoo` and ordinal 3 `fwdord` forwarded to `NTDLL.#7`.
fn add_exports(d: &mut [u8]) {
    set_directory(
        d,
        DIR_EXPORT,
        EXPORT_DIRECTORY,
        EXPORT_DIRECTORY_END - EXPORT_DIRECTORY,
    );
    let (addresses, names, ordinals) = (0x1378, 0x1384, 0x1390);
    let (dll, local, fwdname, fwdord) = (0x1398, 0x13A0, 0x13A8, 0x13B0);
    let (to_name, to_ordinal) = (0x13B8, 0x13C8);
    let ed = at(EXPORT_DIRECTORY);
    put32(d, ed + 12, dll);
    put32(d, ed + 16, 1); // ordinal base
    put32(d, ed + 20, 3); // functions
    put32(d, ed + 24, 3); // names
    put32(d, ed + 28, addresses);
    put32(d, ed + 32, names);
    put32(d, ed + 36, ordinals);
    // A forwarder is an address inside the export directory.
    for (i, rva) in [ENTRY, to_name, to_ordinal].into_iter().enumerate() {
        put32(d, at(addresses) + i * 4, rva);
    }
    // The name pointer table is sorted; each name maps to its ordinal index.
    for (i, (name, index)) in [(fwdname, 1u16), (fwdord, 2), (local, 0)]
        .into_iter()
        .enumerate()
    {
        put32(d, at(names) + i * 4, name);
        put16(d, at(ordinals) + i * 2, index);
    }
    put_bytes(d, dll, b"t.dll\0");
    put_bytes(d, local, b"local\0");
    put_bytes(d, fwdname, b"fwdname\0");
    put_bytes(d, fwdord, b"fwdord\0");
    put_bytes(d, to_name, b"NTDLL.RtlFoo\0");
    put_bytes(d, to_ordinal, b"NTDLL.#7\0");
}

#[test]
fn reads_delay_imports_with_their_iat_slots() {
    let mut d = image();
    add_delay_import(&mut d);
    let info = ImageInfo::parse(&d).unwrap();
    assert_eq!(info.delay_imports.len(), 1);
    let user32 = &info.delay_imports[0];
    assert_eq!(user32.dll, "USER32.dll");
    assert_eq!(user32.entries.len(), 1);
    assert_eq!(
        user32.entries[0].symbol,
        ImportSymbol::Name {
            hint: 7,
            name: "MessageBoxW".into()
        }
    );
    assert_eq!(user32.entries[0].iat_slot_rva, DELAY_IAT);
    // Delay imports are kept apart from the ordinary imports.
    assert_eq!(info.import_count(), 2);
}

#[test]
fn rejects_delay_iat_outside_image() {
    for iat in [SIZE_OF_IMAGE - 4, 0xFFFF_FFFC] {
        let mut d = image();
        add_delay_import(&mut d);
        put32(&mut d, at(DELAY_DESCRIPTORS) + 12, iat);
        assert!(
            matches!(ImageInfo::parse(&d), Err(PeError::Malformed(_))),
            "delay IAT at {iat:#x}"
        );
    }
}

#[test]
fn reads_exports_and_forwarders() {
    let mut d = image();
    add_exports(&mut d);
    let info = ImageInfo::parse(&d).unwrap();
    let mut exports: Vec<_> = info
        .exports
        .iter()
        .map(|e| (e.ordinal, e.name.as_deref(), e.rva, e.forward.as_deref()))
        .collect();
    exports.sort();
    assert_eq!(
        exports,
        [
            (1, Some("local"), Some(ENTRY), None),
            (2, Some("fwdname"), None, Some("NTDLL.RtlFoo")),
            (3, Some("fwdord"), None, Some("NTDLL.#7")),
        ]
    );
}

#[test]
fn reads_codeview_record_and_symbol_key() {
    let mut d = image();
    add_codeview(&mut d, b"RSDS");
    let cv = ImageInfo::parse(&d).unwrap().codeview.unwrap();
    assert_eq!(cv.pdb_path, "t.pdb");
    assert_eq!(cv.age, 0x2A);
    let guid: Vec<u8> = (0..16).collect();
    assert_eq!(cv.guid[..], guid[..]);
    // The first three GUID fields are little endian; the age is hex, unpadded.
    assert_eq!(cv.symbol_key(), "030201000504070608090A0B0C0D0E0F2A");
}

#[test]
fn ignores_codeview_records_that_are_not_rsds() {
    let mut d = image();
    add_codeview(&mut d, b"NB10");
    assert!(ImageInfo::parse(&d).unwrap().codeview.is_none());
}

#[test]
fn reports_relocation_exception_and_load_config_directories() {
    let mut d = image();
    // Only the directory entries matter; their contents are not parsed.
    set_directory(&mut d, dir::BASERELOC, 0x1380, 12);
    set_directory(&mut d, dir::EXCEPTION, 0x1390, 12);
    set_directory(&mut d, dir::LOAD_CONFIG, 0x13A0, 0x40);
    let info = ImageInfo::parse(&d).unwrap();
    assert!(info.has_relocations);
    assert!(info.has_exception_table);
    assert!(info.has_load_config);

    // Stripped relocations win over a leftover directory entry.
    let fh = layout::file_header_offset(&d).unwrap();
    let at_flags = fh + layout::FILE_HEADER_CHARACTERISTICS;
    let flags = u16::from_le_bytes([d[at_flags], d[at_flags + 1]]);
    put16(
        &mut d,
        at_flags,
        flags | layout::file_flags::RELOCS_STRIPPED,
    );
    assert!(!ImageInfo::parse(&d).unwrap().has_relocations);

    // An entry with an RVA but no size is absent.
    set_directory(&mut d, dir::EXCEPTION, 0x1390, 0);
    assert!(!ImageInfo::parse(&d).unwrap().has_exception_table);
}

#[test]
fn names_subsystems_and_dll_flag() {
    use layout::subsystem::*;
    for (value, name) in [
        (WINDOWS_GUI, "windows-gui"),
        (WINDOWS_CUI, "windows-cui"),
        (XBOX, "xbox"),
        (XBOX_CODE_CATALOG, "xbox-code-catalog"),
        (1, "other"),
    ] {
        let mut d = image();
        put16(&mut d, OPT + 68, value);
        let info = ImageInfo::parse(&d).unwrap();
        assert_eq!((info.subsystem, info.subsystem_name()), (value, name));
    }
    let mut d = image();
    put16(&mut d, 0x56, 0x0022 | layout::file_flags::DLL);
    assert!(ImageInfo::parse(&d).unwrap().is_dll());
}

#[test]
fn layout_offsets_match_the_image() {
    let mut d = image();
    assert_eq!(layout::file_header_offset(&d), Some(0x44));
    assert_eq!(layout::optional_header_offset(&d), Some(OPT));
    assert_eq!(
        layout::data_directory_offset(&d, DIR_TLS),
        Some(OPT + layout::OPT_DATA_DIRECTORY + DIR_TLS * layout::DATA_DIRECTORY_SIZE)
    );
    assert_eq!(layout::data_directory_offset(&d, 16), None);
    let tls = layout::data_directory_offset(&d, DIR_TLS).unwrap();
    assert_eq!(layout::read_u32(&d, tls), Some(TLS_DIRECTORY));

    // Fewer directories than the index: none.
    put32(&mut d, OPT + layout::OPT_NUMBER_OF_RVA_AND_SIZES, 2);
    assert!(layout::data_directory_offset(&d, dir::IMPORT).is_some());
    assert_eq!(layout::data_directory_offset(&d, DIR_TLS), None);

    // Headers that point past the data, or nowhere at all, give None.
    assert_eq!(layout::file_header_offset(&d[..0x3C]), None);
    put32(&mut d, layout::DOS_E_LFANEW, u32::MAX);
    assert_eq!(layout::data_directory_offset(&d, dir::IMPORT), None);
}

#[test]
fn reads_names_from_the_iat_when_the_name_table_is_absent() {
    // Old linkers leave OriginalFirstThunk at 0; the IAT holds the names.
    let mut d = image();
    put32(&mut d, at(IMPORT_DESCRIPTORS), 0);
    d[at(NAME_TABLE)..at(NAME_TABLE) + 16].fill(0);
    let info = ImageInfo::parse(&d).unwrap();
    let entries: Vec<_> = info.imports[0]
        .entries
        .iter()
        .map(|e| (e.symbol.clone(), e.iat_slot_rva))
        .collect();
    assert_eq!(
        entries,
        [
            (
                ImportSymbol::Name {
                    hint: 0x123,
                    name: "ExitProcess".into()
                },
                IAT
            ),
            (ImportSymbol::Ordinal(5), IAT + 8),
        ]
    );
}

#[test]
fn tls_directory_without_callbacks_or_without_an_rva() {
    // A TLS directory with no callback array is still TLS (static data).
    let mut d = image();
    put64(
        &mut d,
        at(TLS_DIRECTORY) + layout::TLS64_ADDRESS_OF_CALLBACKS,
        0,
    );
    let tls = ImageInfo::parse(&d).unwrap().tls.unwrap();
    assert_eq!((tls.callbacks_va, tls.callback_rvas.len()), (0, 0));

    // A directory entry with RVA 0 means no TLS at all.
    let mut d = image();
    set_directory(&mut d, DIR_TLS, 0, 40);
    assert!(ImageInfo::parse(&d).unwrap().tls.is_none());

    // So does an image whose header lists no TLS directory slot.
    let mut d = image();
    put32(
        &mut d,
        OPT + layout::OPT_NUMBER_OF_RVA_AND_SIZES,
        DIR_TLS as u32,
    );
    assert!(ImageInfo::parse(&d).unwrap().tls.is_none());
}

#[test]
fn tls_callback_array_stops_at_the_end_of_the_section() {
    // The last slot of the section, with no null terminator after it.
    let last = SECTION_RVA + SECTION_SIZE - 8;
    let mut d = image();
    put64(
        &mut d,
        at(TLS_DIRECTORY) + layout::TLS64_ADDRESS_OF_CALLBACKS,
        IMAGE_BASE + u64::from(last),
    );
    put64(&mut d, at(last), IMAGE_BASE + 0x1030);
    let tls = ImageInfo::parse(&d).unwrap().tls.unwrap();
    assert_eq!(tls.callback_rvas, [0x1030]);
}

#[test]
fn rejects_tls_outside_the_sections() {
    let malformed = |d: &[u8], what: &str| {
        assert!(
            matches!(ImageInfo::parse(d), Err(PeError::Malformed(_))),
            "{what}"
        );
    };
    let callbacks = at(TLS_DIRECTORY) + layout::TLS64_ADDRESS_OF_CALLBACKS;

    // Inside the image but past the only section.
    let mut d = image();
    set_directory(&mut d, DIR_TLS, SECTION_RVA + SECTION_SIZE + 0x100, 40);
    malformed(&d, "directory past the section");

    // Too close to the end of the section to hold AddressOfCallBacks.
    let mut d = image();
    set_directory(&mut d, DIR_TLS, SECTION_RVA + SECTION_SIZE - 0x10, 40);
    malformed(&d, "truncated directory");

    let mut d = image();
    put64(
        &mut d,
        callbacks,
        IMAGE_BASE + u64::from(SECTION_RVA + SECTION_SIZE + 0x100),
    );
    malformed(&d, "callback array past the section");

    let mut d = image();
    put64(&mut d, callbacks, IMAGE_BASE - 8);
    malformed(&d, "callback array below the image base");

    // A callback 4 GiB or more above the base has no RVA.
    let mut d = image();
    put64(&mut d, at(TLS_CALLBACKS) + 8, IMAGE_BASE + (1 << 32));
    malformed(&d, "callback past 4 GiB");
}

#[test]
fn rejects_pe32_images() {
    // Same image, 32-bit optional header magic: PE32 is not supported.
    let mut d = image();
    put16(&mut d, OPT, 0x10B);
    assert!(matches!(ImageInfo::parse(&d), Err(PeError::Unsupported(_))));
}

#[test]
fn import_symbols_display_as_their_name_or_ordinal() {
    let info = ImageInfo::parse(&image()).unwrap();
    let shown: Vec<_> = info.imports[0]
        .entries
        .iter()
        .map(|e| e.symbol.to_string())
        .collect();
    assert_eq!(shown, ["ExitProcess", "#5"]);
}

#[test]
fn tls_callbacks_are_capped() {
    // Grow the section so a 1100-entry callback array, with no terminator,
    // fits after the existing contents.
    const SIZE: u32 = 0x3000;
    const ARRAY: u32 = 0x1400;
    const COUNT: usize = 1100;
    let mut d = image();
    d.resize((SECTION_FILE_OFFSET + SIZE) as usize, 0);
    let sec = OPT + 240;
    put32(&mut d, sec + 8, SIZE);
    put32(&mut d, sec + 16, SIZE);
    put32(&mut d, OPT + 56, SECTION_RVA + SIZE);
    put64(
        &mut d,
        at(TLS_DIRECTORY) + layout::TLS64_ADDRESS_OF_CALLBACKS,
        IMAGE_BASE + u64::from(ARRAY),
    );
    for i in 0..COUNT {
        put64(&mut d, at(ARRAY) + i * 8, IMAGE_BASE + 0x1000 + i as u64);
    }
    let tls = ImageInfo::parse(&d).unwrap().tls.unwrap();
    assert_eq!(tls.callback_rvas.len(), 1024);
    assert_eq!(tls.callback_rvas[0], 0x1000);
    assert_eq!(tls.callback_rvas[1023], 0x1000 + 1023);
}

#[test]
fn reads_exports_without_a_name() {
    // Only two names: the export at ordinal 1 (`local`) is by ordinal only.
    let mut d = image();
    add_exports(&mut d);
    put32(&mut d, at(EXPORT_DIRECTORY) + 24, 2);
    let info = ImageInfo::parse(&d).unwrap();
    let mut exports: Vec<_> = info
        .exports
        .iter()
        .map(|e| (e.ordinal, e.name.as_deref(), e.rva, e.forward.as_deref()))
        .collect();
    exports.sort();
    assert_eq!(
        exports,
        [
            (1, None, Some(ENTRY), None),
            (2, Some("fwdname"), None, Some("NTDLL.RtlFoo")),
            (3, Some("fwdord"), None, Some("NTDLL.#7")),
        ]
    );
}

#[test]
fn reads_delay_imports_by_ordinal() {
    let mut d = image();
    add_delay_import(&mut d);
    put64(&mut d, at(DELAY_NAME_TABLE), (1 << 63) | 9);
    let info = ImageInfo::parse(&d).unwrap();
    let user32 = &info.delay_imports[0];
    assert_eq!(user32.entries.len(), 1);
    assert_eq!(user32.entries[0].symbol, ImportSymbol::Ordinal(9));
    assert_eq!(user32.entries[0].symbol.to_string(), "#9");
    assert_eq!(user32.entries[0].iat_slot_rva, DELAY_IAT);
}

#[test]
fn rva_to_file_offset_picks_the_section_that_holds_the_rva() {
    // A second section, `.data` at RVA 0x2000 and file offset 0x600, with a
    // virtual size of 0: its raw size is then its size in memory.
    const DATA_RVA: u32 = 0x2000;
    const DATA_FILE_OFFSET: u32 = SECTION_FILE_OFFSET + SECTION_SIZE;
    const DATA_SIZE: u32 = 0x200;
    let mut d = image();
    d.resize((DATA_FILE_OFFSET + DATA_SIZE) as usize, 0);
    put16(&mut d, 0x46, 2);
    put32(&mut d, OPT + 56, DATA_RVA + 0x1000);
    let sec = OPT + 240 + 40;
    d[sec..sec + 5].copy_from_slice(b".data");
    put32(&mut d, sec + 12, DATA_RVA);
    put32(&mut d, sec + 16, DATA_SIZE);
    put32(&mut d, sec + 20, DATA_FILE_OFFSET);
    put32(&mut d, sec + 36, 0xC000_0040); // initialised data, read, write

    let info = ImageInfo::parse(&d).unwrap();
    let sections: Vec<_> = info
        .sections
        .iter()
        .map(|s| (s.name.trim_end_matches('\0'), s.rva, s.virtual_size))
        .collect();
    assert_eq!(
        sections,
        [(".text", SECTION_RVA, SECTION_SIZE), (".data", DATA_RVA, 0)]
    );
    assert_eq!(
        info.rva_to_file_offset(HINT_NAME),
        Some(at(HINT_NAME) as u32)
    );
    assert_eq!(info.rva_to_file_offset(DATA_RVA), Some(DATA_FILE_OFFSET));
    assert_eq!(
        info.rva_to_file_offset(DATA_RVA + DATA_SIZE - 1),
        Some(DATA_FILE_OFFSET + DATA_SIZE - 1)
    );
    // The gap between the sections and the end of `.data` map nowhere.
    assert_eq!(
        info.rva_to_file_offset(SECTION_RVA + SECTION_SIZE + 0x10),
        None
    );
    assert_eq!(info.rva_to_file_offset(DATA_RVA + DATA_SIZE), None);
}

#[test]
fn rejects_damaged_dos_and_nt_signatures() {
    // No `MZ`: not a PE file at all.
    let mut d = image();
    d[0..2].copy_from_slice(b"ZM");
    assert!(ImageInfo::parse(&d).is_err());

    // `MZ`, but the NT signature is wrong.
    let mut d = image();
    d[0x40..0x44].copy_from_slice(b"NE\0\0");
    assert!(ImageInfo::parse(&d).is_err());

    // `e_lfanew` points past the end of the file, or right at its last bytes.
    for lfanew in [u32::MAX, d.len() as u32, d.len() as u32 - 2] {
        let mut d = image();
        put32(&mut d, layout::DOS_E_LFANEW, lfanew);
        assert!(ImageInfo::parse(&d).is_err(), "e_lfanew {lfanew:#x}");
    }
}

#[test]
fn rejects_an_import_name_outside_the_sections() {
    for name in [SIZE_OF_IMAGE + 0x100, 0xFFFF_FFF0] {
        let mut d = image();
        put32(&mut d, at(IMPORT_DESCRIPTORS) + 12, name);
        assert!(ImageInfo::parse(&d).is_err(), "DLL name at {name:#x}");
    }
    for hint_name in [SIZE_OF_IMAGE + 0x100, 0x7FFF_FFF0] {
        let mut d = image();
        put64(&mut d, at(NAME_TABLE), u64::from(hint_name));
        assert!(ImageInfo::parse(&d).is_err(), "hint/name at {hint_name:#x}");
    }
}

#[test]
fn import_and_tls_directories_pointing_outside_the_image_do_not_panic() {
    for rva in [SIZE_OF_IMAGE, SIZE_OF_IMAGE + 0x1000, 0xFFFF_FFF0] {
        for index in [
            dir::IMPORT,
            DIR_TLS,
            DIR_EXPORT,
            DIR_DEBUG,
            DIR_DELAY_IMPORT,
        ] {
            let mut d = image();
            set_directory(&mut d, index, rva, 0x40);
            // Either outcome is acceptable; the parse must simply return.
            let _ = ImageInfo::parse(&d);
        }
    }
}

#[test]
fn corrupting_any_header_byte_does_not_panic() {
    // Every byte of the headers, the section table and the directories in
    // the section, set to each of a few hostile values.
    let clean = image();
    let ranges = [
        0..OPT + 240 + 40,
        at(IMPORT_DESCRIPTORS)..at(HINT_NAME) + 16,
        at(TLS_DIRECTORY)..at(TLS_CALLBACKS) + 16,
    ];
    for range in ranges {
        for i in range {
            for v in [0x00, 0x7F, 0x80, 0xFF] {
                if clean[i] == v {
                    continue;
                }
                let mut d = clean.clone();
                d[i] = v;
                let _ = ImageInfo::parse(&d);
            }
        }
    }
}

#[test]
fn every_truncation_of_the_image_returns() {
    let d = image();
    for len in 0..d.len() {
        let _ = ImageInfo::parse(&d[..len]);
    }
}
