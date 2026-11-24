//! `SGD5`: the engine's shader file. Each record holds one shader container,
//! deflate-packed. The header tables are not decoded yet, so records are
//! found by their marker and cross-checked by their sizes:
//!
//! ```text
//! u8 name length (with NUL), name, NUL, u8 kind,
//! u32 n, u32 0x000DDCE7, u32 n, u32 unpacked size, raw deflate (n - 12 bytes)
//! ```

pub const MAGIC: &[u8; 4] = b"SGD5";
const MARKER: [u8; 4] = 0x000D_DCE7_u32.to_le_bytes();
/// Containers are at most a few MiB; refuse to allocate for absurd sizes.
const MAX_UNPACKED: usize = 64 << 20;
/// Bytes a record's packed stream may hold beyond what deflate needs (see
/// [`max_packed`]): the stream's end is not checked, so allow some padding.
const PACKED_SLACK: usize = 4096;

/// The most packed bytes a container of `unpacked` bytes can take. Deflate
/// never spends more than 9 bits a byte (fixed codes), and stored blocks only
/// 5 bytes per 65535, so a size beyond this is not a record's: it is bytes
/// that look like a marker (in a container stored between records, say).
fn max_packed(unpacked: usize) -> usize {
    unpacked + unpacked / 8 + PACKED_SLACK
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// Offset of the marker in the file.
    pub offset: usize,
    /// Like `PS_<name>_<hash>_Wrapped`; `None` if the bytes before the
    /// record do not read as one.
    pub name: Option<&'a str>,
    /// Record type, meaning unknown (0x32 most shaders, 0x41 mesh shaders
    /// and some material families, 0x3c compute, 0x40, 0x42).
    pub kind: u8,
    packed: &'a [u8],
    unpacked_size: usize,
}

impl Record<'_> {
    /// The container, or `None` if the stream is corrupt.
    pub fn unpack(&self) -> Option<Vec<u8>> {
        miniz_oxide::inflate::decompress_to_vec_with_limit(self.packed, self.unpacked_size)
            .ok()
            .filter(|out| out.len() == self.unpacked_size)
    }

    /// The file bytes from the marker to the end of the packed stream.
    pub fn span(&self) -> std::ops::Range<usize> {
        self.offset..self.offset + 12 + self.packed.len()
    }
}

/// Whether `offset` lies inside one of `records`, taken in the order
/// [`records`] yields them (file order, disjoint). Bytes there belong to a
/// record: a container stored as is inside its packed stream (deflate stored
/// blocks) is that record's container, not another one.
pub fn in_record(records: &[Record<'_>], offset: usize) -> bool {
    let i = records.partition_point(|r| r.span().end <= offset);
    records.get(i).is_some_and(|r| r.span().contains(&offset))
}

/// An SGD5 file, or a file holding one (an archive: records are found by
/// their marker, so no index is needed).
pub fn is_sgd5(file: &[u8]) -> bool {
    memchr::memmem::find(file, MAGIC).is_some()
}

/// Every record in an SGD5 file, in file order. The search starts at the
/// first magic: bytes before it (an archive's other files) belong to no SGD5
/// file, whatever markers they happen to hold. It resumes after each
/// record's packed bytes: a marker inside them (stored deflate blocks carry a
/// container's bytes as they are) is part of that record, and so is any part
/// of the next record's header or name, which must follow them. A record
/// whose packed bytes run past the end of the file (a truncated file) is not
/// yielded, and ends the search: every byte after its marker is its own.
/// Sizes that no deflate stream of the stated unpacked size could have do
/// not form a record, and neither do bytes running past the end that do not
/// start a deflate stream, so such bytes neither end the search nor hide
/// records.
pub fn records(file: &[u8]) -> impl Iterator<Item = Record<'_>> {
    let finder = memchr::memmem::Finder::new(&MARKER);
    // Where the next record may start: after the magic, then after each
    // record's packed bytes. No magic: past the end, so nothing is found.
    let mut floor = memchr::memmem::find(file, MAGIC).map_or(file.len(), |m| m + MAGIC.len());
    // A marker needs its name's NUL, the kind and `n` (6 bytes) before it.
    let mut from = floor + 6;
    std::iter::from_fn(move || {
        while let Some(found) = finder.find(file.get(from..)?) {
            let at = from + found;
            match record(&file[floor..], at - floor) {
                Found::Record(r) => {
                    let r = Record { offset: at, ..r };
                    floor = r.span().end;
                    from = floor + 6;
                    return Some(r);
                }
                Found::Truncated => {
                    from = file.len() + 1;
                    return None;
                }
                Found::Nothing => from = at + 1,
            }
        }
        None
    })
}

enum Found<'a> {
    Record(Record<'a>),
    /// A record header whose packed bytes run past the end of the file.
    Truncated,
    Nothing,
}

/// The record whose marker is at `at`, reading nothing before `file`.
fn record(file: &[u8], at: usize) -> Found<'_> {
    let u32_at = |o: usize| {
        file.get(o..o.checked_add(4)?)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize)
    };
    let Some(nul) = at.checked_sub(6) else {
        return Found::Nothing;
    };
    let (Some(n), Some(n2), Some(unpacked_size)) = (u32_at(at - 4), u32_at(at + 4), u32_at(at + 8))
    else {
        return Found::Nothing;
    };
    if n2 != n || n < 12 || unpacked_size > MAX_UNPACKED || n - 12 > max_packed(unpacked_size) {
        return Found::Nothing;
    }
    let Some(packed) = at.checked_add(n).and_then(|end| file.get(at + 12..end)) else {
        // Bytes that only look like a marker (in a container stored between
        // records, say) may have sizes that fit and still run past the end.
        // Only a record whose bytes so far are a deflate stream is one.
        return if starts_stream(file.get(at + 12..).unwrap_or_default(), unpacked_size) {
            Found::Truncated
        } else {
            Found::Nothing
        };
    };
    Found::Record(Record {
        offset: at,
        name: (file[nul] == 0).then(|| name_before(file, nul)).flatten(),
        kind: file[at - 5],
        packed,
        unpacked_size,
    })
}

/// Whether `bytes` are the start of a deflate stream of `unpacked` bytes:
/// they run out before the stream ends, or the stream ends having produced
/// exactly that many bytes.
fn starts_stream(bytes: &[u8], unpacked: usize) -> bool {
    use miniz_oxide::inflate::{TINFLStatus, decompress_to_vec_with_limit};
    match decompress_to_vec_with_limit(bytes, unpacked) {
        Ok(out) => out.len() == unpacked,
        Err(e) => e.status == TINFLStatus::FailedCannotMakeProgress,
    }
}

/// The printable name that ends at `nul` and follows its length byte. A
/// length byte of 1 (no characters) is not a name: any `01 00` before the
/// kind would read as one.
fn name_before(file: &[u8], nul: usize) -> Option<&str> {
    let run = file[..nul]
        .iter()
        .rposition(|b| !b.is_ascii_graphic())
        .map_or(0, |p| p + 1);
    // From 32 characters on, the length byte is printable too.
    (run.saturating_sub(1)..nul)
        .find(|&l| usize::from(file[l]) == nul - l)
        .and_then(|l| std::str::from_utf8(&file[l + 1..nul]).ok())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_bytes(name: &str, kind: u8, payload: &[u8]) -> Vec<u8> {
        let packed = miniz_oxide::deflate::compress_to_vec(payload, 6);
        let n = (packed.len() + 12) as u32;
        let mut out = vec![0xF4, 0x01, 0, 0, name.len() as u8 + 1];
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&[0, kind]);
        for v in [n, 0x000D_DCE7, n, payload.len() as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&packed);
        out
    }

    #[test]
    fn reads_records() {
        let long = format!("PS_{}_Wrapped", "x".repeat(40));
        let mut file = MAGIC.to_vec();
        file.extend_from_slice(&[1, 2, 3, 4]);
        file.extend(record_bytes(
            "VS_Blit_0011aabb_Wrapped",
            0x32,
            b"first container",
        ));
        // A marker whose sizes disagree is not a record.
        file.extend_from_slice(&[9, 0, 0, 0, 0xE7, 0xDC, 0x0D, 0, 8, 0, 0, 0]);
        file.extend(record_bytes(&long, 0x41, &[7; 300]));

        assert!(is_sgd5(&file));
        let found: Vec<_> = records(&file).collect();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, Some("VS_Blit_0011aabb_Wrapped"));
        assert_eq!(found[0].kind, 0x32);
        assert_eq!(found[0].unpack().as_deref(), Some(&b"first container"[..]));
        assert_eq!(found[1].name, Some(long.as_str()));
        assert_eq!(found[1].unpack(), Some(vec![7; 300]));

        // The same file inside an archive.
        let mut archive = vec![0x5A; 0x1000];
        archive.extend_from_slice(&file);
        assert!(is_sgd5(&archive));
        assert_eq!(records(&archive).count(), 2);
        assert!(!is_sgd5(&archive[..0x1000]));
    }

    #[test]
    fn marker_inside_a_record_is_not_another_record() {
        let inner = record_bytes("PS_inner", 0x32, b"inner container");
        // Level 0: stored blocks, so the inner record's bytes appear as they are.
        let packed = miniz_oxide::deflate::compress_to_vec(&inner, 0);
        assert!(memchr::memmem::find(&packed, &MARKER).is_some());
        let n = (packed.len() + 12) as u32;
        let mut file = MAGIC.to_vec();
        file.extend_from_slice(&[0xF4, 0x01, 0, 0, 9]);
        file.extend_from_slice(b"PS_outer\0\x32");
        for v in [n, 0x000D_DCE7, n, inner.len() as u32] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        file.extend_from_slice(&packed);
        file.extend(record_bytes("PS_after", 0x32, b"next"));

        let found: Vec<_> = records(&file).collect();
        let names: Vec<_> = found.iter().map(|r| r.name).collect();
        assert_eq!(names, [Some("PS_outer"), Some("PS_after")]);
        assert_eq!(found[0].unpack(), Some(inner));
    }

    #[test]
    fn offsets_inside_records() {
        // A container stored as is (level 0) inside a record's packed bytes.
        let container = b"DXBC stored as is";
        let mut file = MAGIC.to_vec();
        file.extend_from_slice(b"\x05PS_a\0\x32");
        let packed = miniz_oxide::deflate::compress_to_vec(container, 0);
        let n = (packed.len() + 12) as u32;
        for v in [n, 0x000D_DCE7, n, container.len() as u32] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        file.extend_from_slice(&packed);
        let between = file.len();
        file.extend_from_slice(b"DXBC between records");
        file.extend(record_bytes("PS_b", 0x32, b"second"));
        let after = file.len();
        file.extend_from_slice(b"DXBC after");

        let found: Vec<_> = records(&file).collect();
        assert_eq!(found.len(), 2);
        let stored = memchr::memmem::find(&file, b"DXBC").unwrap();
        assert!(stored < between);
        assert!(in_record(&found, stored));
        assert!(!in_record(&found, 0), "the magic");
        assert!(!in_record(&found, between));
        assert!(!in_record(&found, after));
        for r in &found {
            let span = r.span();
            assert!(in_record(&found, span.start));
            assert!(in_record(&found, span.end - 1));
            assert!(!in_record(&found, span.start - 1), "the header");
        }
        assert!(!in_record(&[], stored));
    }

    #[test]
    fn header_inside_a_record_is_not_another_record() {
        // The record's packed bytes end (after the deflate stream, which
        // ignores them) with what would be the next header: NUL, kind, `n`.
        let mut packed = miniz_oxide::deflate::compress_to_vec(b"first", 6);
        let tail = miniz_oxide::deflate::compress_to_vec(b"phantom", 6);
        let tail_n = (tail.len() + 12) as u32;
        packed.extend_from_slice(&[0, 0x32]);
        packed.extend_from_slice(&tail_n.to_le_bytes());
        let n = (packed.len() + 12) as u32;
        let mut file = MAGIC.to_vec();
        file.extend_from_slice(b"\x09PS_first\0\x32");
        for v in [n, 0x000D_DCE7, n, 5] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        file.extend_from_slice(&packed);
        // Right after it, a marker with no header of its own.
        for v in [0x000D_DCE7, tail_n, 7] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        file.extend_from_slice(&tail);

        let found: Vec<_> = records(&file).collect();
        let names: Vec<_> = found.iter().map(|r| r.name).collect();
        assert_eq!(names, [Some("PS_first")]);
        assert_eq!(found[0].unpack().as_deref(), Some(&b"first"[..]));
    }

    #[test]
    fn records_before_the_magic_are_not_read() {
        // An archive whose other file happens to hold a well-formed record.
        let mut archive = record_bytes("PS_elsewhere", 0x32, b"not ours");
        assert_eq!(records(&archive).count(), 0, "no magic, no records");
        archive.extend_from_slice(MAGIC);
        archive.extend(record_bytes("PS_ours", 0x32, b"ours"));
        let names: Vec<_> = records(&archive).map(|r| r.name).collect();
        assert_eq!(names, [Some("PS_ours")]);
    }

    #[test]
    fn truncated_file_yields_only_whole_records() {
        let mut file = MAGIC.to_vec();
        file.extend(record_bytes("PS_a", 0x32, &[5; 200]));
        let first_end = file.len();
        file.extend(record_bytes("PS_b", 0x41, b"second"));
        for cut in 0..=file.len() {
            let part = &file[..cut];
            let found: Vec<_> = records(part).collect();
            let expected = usize::from(cut >= first_end) + usize::from(cut == file.len());
            assert_eq!(found.len(), expected, "cut at {cut}");
            for r in found {
                assert!(
                    r.unpack().is_some(),
                    "cut at {cut}: {:?} does not unpack",
                    r.name
                );
            }
        }
    }

    #[test]
    fn truncated_record_hides_the_records_it_stores() {
        // A record whose packed bytes (stored blocks) hold a whole record,
        // cut off after that inner record but before its own end.
        let inner = record_bytes("PS_inner", 0x32, b"inner container");
        let mut payload = inner.clone();
        payload.extend_from_slice(&[0x55; 64]);
        let packed = miniz_oxide::deflate::compress_to_vec(&payload, 0);
        let n = (packed.len() + 12) as u32;
        let mut file = MAGIC.to_vec();
        file.extend(record_bytes("PS_first", 0x32, b"first"));
        file.extend_from_slice(b"\x09PS_outer\0\x32");
        for v in [n, 0x000D_DCE7, n, payload.len() as u32] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        let stored = file.len();
        file.extend_from_slice(&packed);
        let inner_end = stored + memchr::memmem::find(&packed, &inner).unwrap() + inner.len();
        let names: Vec<_> = records(&file).map(|r| r.name).collect();
        assert_eq!(names, [Some("PS_first"), Some("PS_outer")]);

        // Before: [PS_first, PS_inner].
        let names: Vec<_> = records(&file[..inner_end]).map(|r| r.name).collect();
        assert_eq!(names, [Some("PS_first")]);
    }

    #[test]
    fn marker_with_impossible_sizes_does_not_end_the_search() {
        // Bytes between records that read as a marker whose packed size runs
        // past the end of the file, but is far more than a deflate stream of
        // 16 bytes could take.
        let mut file = MAGIC.to_vec();
        file.extend_from_slice(b"DXBC");
        for v in [u32::MAX, 0x000D_DCE7, u32::MAX, 16] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        file.extend(record_bytes("PS_after", 0x32, b"after"));
        // Before: [] (the phantom was a truncated record and ended the search).
        let names: Vec<_> = records(&file).map(|r| r.name).collect();
        assert_eq!(names, [Some("PS_after")]);
    }

    #[test]
    fn marker_running_past_the_end_without_a_stream_does_not_end_the_search() {
        // Bytes between records that read as a marker with sizes that fit
        // (64 KiB both ways) and run past the end of the file, followed by
        // bytes that are no deflate stream (block type 3 is reserved).
        let mut file = MAGIC.to_vec();
        file.extend(record_bytes("PS_first", 0x32, b"first"));
        file.extend_from_slice(b"DXBC");
        for v in [0x1_0000_u32, 0x000D_DCE7, 0x1_0000, 0x1_0000] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        file.extend_from_slice(&[0x07; 16]);
        file.extend(record_bytes("PS_after", 0x32, b"after"));
        // Before: [PS_first] (the phantom was a truncated record and ended
        // the search).
        let names: Vec<_> = records(&file).map(|r| r.name).collect();
        assert_eq!(names, [Some("PS_first"), Some("PS_after")]);
    }

    #[test]
    fn truncated_stream_is_told_from_noise() {
        let payload: Vec<u8> = (0..4000u32).map(|i| (i * 7 % 251) as u8).collect();
        for level in [0, 1, 6, 10] {
            let packed = miniz_oxide::deflate::compress_to_vec(&payload, level);
            for cut in 0..packed.len() {
                assert!(
                    starts_stream(&packed[..cut], payload.len()),
                    "level {level}, cut at {cut}"
                );
            }
            assert!(starts_stream(&packed, payload.len()), "level {level}");
            // A whole stream of another size is not this record's.
            assert!(!starts_stream(&packed, payload.len() + 1), "level {level}");
            assert!(!starts_stream(&packed, payload.len() - 1), "level {level}");
        }
        // Reserved block type, and a stored block whose length check fails.
        assert!(!starts_stream(&[0x07, 0, 0], 100));
        assert!(!starts_stream(&[0x01, 0x05, 0x00, 0x00, 0x00, 1, 2, 3], 5));
    }

    #[test]
    fn packed_size_bound_holds_for_every_level() {
        // Incompressible bytes, at every level miniz writes: the bound is
        // never reached, so no real record is refused.
        let mut x = 0x1234_5678_u32;
        let noise: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        for len in [0, 1, 100, 65_535, 65_536, noise.len()] {
            for level in 0..=10 {
                let packed = miniz_oxide::deflate::compress_to_vec(&noise[..len], level);
                assert!(packed.len() <= max_packed(len), "len {len} level {level}");
            }
        }
    }

    #[test]
    fn empty_container_unpacks() {
        let mut file = MAGIC.to_vec();
        file.extend(record_bytes("PS_empty", 0x32, b""));
        let r = records(&file).next().unwrap();
        assert_eq!(r.unpack(), Some(Vec::new()));
    }

    /// Every single-byte change to a small file: no panic, and the records
    /// found are in file order, inside the file and do not overlap.
    #[test]
    fn damaged_file_yields_disjoint_records() {
        let mut file = MAGIC.to_vec();
        file.extend(record_bytes("PS_a", 0x32, &[5; 40]));
        file.extend(record_bytes("VS_b", 0x41, b"second"));
        for at in 0..file.len() {
            for value in [0x00, 0x01, 0x0C, 0x0D, 0x7F, 0xDC, 0xE7, 0xFF] {
                let mut damaged = file.clone();
                damaged[at] = value;
                let mut end = 0;
                for r in records(&damaged) {
                    let start = r.offset - 6;
                    assert!(start >= end, "byte {at} = {value:#x}: records overlap");
                    end = r.span().end;
                    assert!(end <= damaged.len());
                    let _ = r.unpack();
                }
            }
        }
    }

    /// Seeded random files: gaps of noise and of near-miss markers between
    /// records, some records stored as is (level 0) and holding records of
    /// their own. Exactly the planted records are found, each with its
    /// container, and [`in_record`] agrees with a scan of their spans.
    #[test]
    fn random_files_yield_exactly_the_planted_records() {
        let mut x = 0x9E37_79B9_u32;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for seed in 0..200 {
            let mut file = MAGIC.to_vec();
            // (offset of the marker, name, container)
            let mut planted: Vec<(usize, String, Vec<u8>)> = Vec::new();
            for i in 0..1 + next() % 6 {
                // A gap: noise, then sometimes a marker whose sizes disagree
                // or that no deflate stream could have.
                for _ in 0..next() % 40 {
                    file.push(next() as u8);
                }
                match next() % 3 {
                    0 => {
                        for v in [20_u32, 0x000D_DCE7, 21, 8] {
                            file.extend_from_slice(&v.to_le_bytes());
                        }
                    }
                    1 => {
                        for v in [u32::MAX, 0x000D_DCE7, u32::MAX, 16] {
                            file.extend_from_slice(&v.to_le_bytes());
                        }
                    }
                    _ => {}
                }
                // A name's length byte follows the gap; a long name's is
                // printable, so noise right before it is kept non-printable.
                file.push(0x01);
                let name = format!("PS_{seed}_{i}_{}", "w".repeat(next() as usize % 50));
                let mut payload: Vec<u8> = (0..next() % 300).map(|_| next() as u8).collect();
                let level = if next() % 2 == 0 {
                    // Stored as is, holding a record and a container's magic.
                    payload.extend(record_bytes("PS_stored", 0x32, b"stored"));
                    payload.extend_from_slice(b"DXBC");
                    0
                } else {
                    6
                };
                let packed = miniz_oxide::deflate::compress_to_vec(&payload, level);
                let n = (packed.len() + 12) as u32;
                file.push(name.len() as u8 + 1);
                file.extend_from_slice(name.as_bytes());
                file.extend_from_slice(&[0, 0x32]);
                planted.push((file.len() + 4, name, payload.clone()));
                for v in [n, 0x000D_DCE7, n, payload.len() as u32] {
                    file.extend_from_slice(&v.to_le_bytes());
                }
                file.extend_from_slice(&packed);
            }
            for _ in 0..next() % 40 {
                file.push(next() as u8);
            }

            let found: Vec<_> = records(&file).collect();
            let got: Vec<_> = found
                .iter()
                .map(|r| (r.offset, r.name.map(str::to_owned), r.unpack()))
                .collect();
            let want: Vec<_> = planted
                .iter()
                .map(|(at, name, payload)| (*at, Some(name.clone()), Some(payload.clone())))
                .collect();
            assert_eq!(got, want, "seed {seed}");
            for offset in 0..=file.len() {
                let linear = found.iter().any(|r| r.span().contains(&offset));
                assert_eq!(
                    in_record(&found, offset),
                    linear,
                    "seed {seed}, offset {offset}"
                );
            }
        }
    }

    #[test]
    fn empty_name_is_no_name() {
        // A record whose name would be empty: `01 00` before the kind.
        let mut file = MAGIC.to_vec();
        file.extend(record_bytes("", 0x32, b"nameless"));
        file.extend(record_bytes("PS_named", 0x32, b"named"));
        let found: Vec<_> = records(&file).collect();
        let names: Vec<_> = found.iter().map(|r| r.name).collect();
        assert_eq!(names, [None, Some("PS_named")]);
        assert_eq!(found[0].unpack().as_deref(), Some(&b"nameless"[..]));
    }

    #[test]
    fn archive_with_two_sgd5_files_yields_both() {
        // Two SGD5 files one after the other in an archive, with another
        // file's bytes between them: the second magic sits in a gap.
        let mut archive = b"other file".to_vec();
        archive.extend_from_slice(MAGIC);
        archive.extend(record_bytes("PS_one", 0x32, b"first file"));
        archive.extend_from_slice(b"another file between");
        archive.extend_from_slice(MAGIC);
        archive.extend(record_bytes("PS_two", 0x41, b"second file"));
        let found: Vec<_> = records(&archive).collect();
        let names: Vec<_> = found.iter().map(|r| r.name).collect();
        assert_eq!(names, [Some("PS_one"), Some("PS_two")]);
        assert_eq!(found[1].kind, 0x41);
        assert_eq!(found[1].unpack().as_deref(), Some(&b"second file"[..]));
    }

    #[test]
    fn corrupt_stream_does_not_unpack() {
        let mut file = MAGIC.to_vec();
        file.extend(record_bytes("CS_x", 0x3c, &[1; 64]));
        file[31] = 0xFF; // first packed byte: reserved block type
        let r = records(&file).next().unwrap();
        assert_eq!(r.unpack(), None);
    }
}
