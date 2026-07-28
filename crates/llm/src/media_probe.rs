//! Facts about a media payload that only the bytes can answer, probed
//! server-side at the one moment the file is in hand.
//!
//! Every fact here is a *price input*, not decoration. A native PDF is
//! billed per page, audio is billed per second and an image is billed per
//! tile of its PIXEL grid, so a `ContentBlock` that carries none of them
//! leaves `baybo-context`'s tokenizer with nothing to charge but a
//! worst-case ceiling. Measured on PDFs from three independent producers,
//! byte count is not a usable stand-in: a 1.5-object-stream document runs
//! anywhere from 10 to 4,007 bytes per page, so a 64 KiB budget admits
//! either 16 pages or 6,872 depending on nothing an ingester can see. The
//! same holds for images — a 5 MiB payload cap admits a 12000x9000 flat
//! render whose tiling costs 49,536 tokens.
//!
//! Every entry point is infallible-by-`Option` and synchronous. Callers
//! on an async path run them under `spawn_blocking`: both parsers are
//! CPU-bound over the whole payload, and a panic inside a blocking task
//! surfaces as a `JoinError` instead of unwinding through the reactor.

use std::io::Cursor;

/// Pages in a PDF, or `None` when the bytes don't parse as one.
///
/// A real parser rather than a `/Type /Page` scan, because PDF 1.5 packs
/// page objects into Flate-compressed object streams where no page
/// object appears in the byte stream at all — measured, a byte scan of a
/// 64-page object-stream document finds zero matches and of a 1,000-page
/// one finds zero.
///
/// **The larger of two readings, because the caller treats this as an
/// UPPER bound and a page-tree walk is a LOWER one.**
/// `Document::get_pages` silently drops pages four ways: a kid that will
/// not resolve or whose `/Type` will not read is skipped, traversal stops
/// after `doc.objects.len()` steps, a subtree past depth 256 is dropped,
/// and a kid that is neither `/Page` nor `/Pages` is ignored. Any
/// reduction that lands inside the deliverable range passes the gate and
/// is BOTH priced low AND delivered, while the provider bills the real
/// count — a 40-page document walked as 5 is charged 39,000 and costs
/// 312,000, re-paid every turn because the request is rebuilt from the
/// whole history. `load_metadata_mem` reads the declared `/Pages /Count`
/// off the catalog without walking anything, so taking the maximum makes
/// the number conservative in the direction the gate claims.
///
/// Each reading is optional on its own: a document whose full load fails
/// can still declare a count, and one with no `/Count` still walks.
/// `None` only when neither parses at all.
pub fn pdf_page_count(bytes: &[u8]) -> Option<u32> {
    let walked = lopdf::Document::load_mem(bytes)
        .ok()
        .and_then(|doc| u32::try_from(doc.get_pages().len()).ok());
    let declared = lopdf::Document::load_metadata_mem(bytes)
        .ok()
        .map(|meta| meta.page_count);
    match (walked, declared) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (found, None) | (None, found) => found,
    }
}

/// Pixel dimensions of an image, or `None` when the bytes are not a
/// raster format we can measure.
///
/// A header parse, not a decode: the providers tile an image by its pixel
/// grid, so `(width, height)` is the whole price input and nothing else in
/// the file is worth reading. Vector formats (`image/svg+xml`) have no
/// pixel dimensions to find, so they answer `None` here and the delivery
/// path refuses them — an SVG can declare a 100000x100000 viewBox in a
/// kilobyte, which is exactly the unbounded price the byte cap failed to
/// bound.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let size = imagesize::blob_size(bytes).ok()?;
    Some((
        u32::try_from(size.width).ok()?,
        u32::try_from(size.height).ok()?,
    ))
}

/// Playback length of an audio payload in milliseconds, or `None` when
/// the container doesn't parse.
///
/// Sniffs the container by CONTENT rather than by the MIME the client
/// declared — an Opus stream inside `audio/ogg` is routine, and trusting
/// the label there is what makes a duration go missing.
pub fn audio_duration_ms(bytes: &[u8]) -> Option<u32> {
    use lofty::file::AudioFile;
    use lofty::probe::Probe;

    let file = Probe::new(Cursor::new(bytes))
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    u32::try_from(file.properties().duration().as_millis()).ok()
}

/// Fixtures are generated rather than checked in: the property under
/// test is that page objects hidden inside a compressed object stream
/// are still found, and a committed binary would make it impossible
/// to see that the fixture really has that shape. Shared with the
/// delivery tests in `crate` and with the gateway's ingest tests, which
/// both need payloads that really parse.
#[cfg(any(test, feature = "test-support"))]
pub mod fixture {
    /// Uncompressed mono PCM WAVE of `seconds`, the one audio container
    /// simple enough to synthesise byte-exactly. Deliberately narrowband
    /// (8 kB/s) so a fixture at the duration cap still fits under the
    /// payload cap and the two gates can be tested apart.
    pub fn wav(seconds: u32) -> Vec<u8> {
        const RATE: u32 = 4_000;
        const BYTES_PER_SAMPLE: u32 = 2;
        let data_len = seconds * RATE * BYTES_PER_SAMPLE;
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&RATE.to_le_bytes());
        out.extend_from_slice(&(RATE * BYTES_PER_SAMPLE).to_le_bytes());
        out.extend_from_slice(&(BYTES_PER_SAMPLE as u16).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(out.len() + data_len as usize, 0);
        out
    }

    /// A PNG that really declares `width` x `height`, IHDR CRC included.
    /// Synthesised rather than checked in for the same reason the PDFs
    /// are: the dimensions under test have to be visible in the fixture,
    /// and a 12000x9000 committed image would be neither reviewable nor
    /// small.
    pub fn png(width: u32, height: u32) -> Vec<u8> {
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut crc = flate2::Crc::new();
        crc.update(&ihdr);

        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(&ihdr);
        out.extend_from_slice(&crc.sum().to_be_bytes());
        out
    }

    /// Minimal classic-xref PDF 1.4 — one page object per page, all
    /// visible in the byte stream.
    pub fn classic(pages: usize) -> Vec<u8> {
        classic_tree(pages, &pages.to_string(), 0)
    }

    /// A document whose page-tree WALK under-reports: `walkable` real page
    /// objects followed by `dangling` `/Kids` entries pointing at objects
    /// that were never written, which `Document::get_pages` skips without
    /// a word. The declared `/Count` still says how many pages the
    /// document has.
    pub fn classic_understating_walk(walkable: usize, declared: u32, dangling: usize) -> Vec<u8> {
        classic_tree(walkable, &declared.to_string(), dangling)
    }

    /// The mirror case: a real page tree under a `/Count` of zero, which
    /// is what a truncated or lying producer leaves behind. The walk is
    /// the only reading that finds the pages.
    pub fn classic_understating_count(pages: usize) -> Vec<u8> {
        classic_tree(pages, "0", 0)
    }

    fn classic_tree(pages: usize, declared: &str, dangling: usize) -> Vec<u8> {
        let mut objs: Vec<(usize, Vec<u8>)> = Vec::new();
        let kids: Vec<usize> = (0..pages).map(|i| 4 + i).collect();
        objs.push((1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()));
        let kid_refs: Vec<String> = kids
            .iter()
            .map(|k| format!("{k} 0 R"))
            .chain((0..dangling).map(|i| format!("{} 0 R", 4 + pages + i)))
            .collect();
        objs.push((
            2,
            format!(
                "<< /Type /Pages /Count {declared} /Kids [{}] >>",
                kid_refs.join(" ")
            )
            .into_bytes(),
        ));
        objs.push((
            3,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ));
        for k in &kids {
            objs.push((
                *k,
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_vec(),
            ));
        }

        let mut out = b"%PDF-1.4\n".to_vec();
        let top = 4 + pages;
        let mut offsets = vec![0usize; top];
        for (num, body) in &objs {
            offsets[*num] = out.len();
            out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_at = out.len();
        out.extend_from_slice(format!("xref\n0 {top}\n0000000000 65535 f \n").as_bytes());
        for off in offsets.iter().take(top).skip(1) {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {top} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n")
                .as_bytes(),
        );
        out
    }

    /// PDF 1.5 — every page object packed into one Flate-compressed
    /// object stream, located through a cross-reference stream. No
    /// `/Type /Page` appears anywhere in the raw bytes.
    pub fn object_stream(pages: usize) -> Vec<u8> {
        use std::io::Write;

        let page_nums: Vec<usize> = (4..4 + pages).collect();
        let objstm_num = 4 + pages;
        let xref_num = objstm_num + 1;
        let top = xref_num + 1;

        let kid_refs: Vec<String> = page_nums.iter().map(|k| format!("{k} 0 R")).collect();
        let mut packed: Vec<(usize, Vec<u8>)> = vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (
                2,
                format!(
                    "<< /Type /Pages /Count {pages} /Kids [{}] >>",
                    kid_refs.join(" ")
                )
                .into_bytes(),
            ),
            (
                3,
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            ),
        ];
        for p in &page_nums {
            packed.push((
                *p,
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_vec(),
            ));
        }
        packed.sort_by_key(|(n, _)| *n);

        let mut pairs = Vec::new();
        let mut bodies: Vec<u8> = Vec::new();
        for (num, body) in &packed {
            pairs.push(format!("{num} {}", bodies.len()));
            bodies.extend_from_slice(body);
            bodies.push(b'\n');
        }
        let header = format!("{}\n", pairs.join(" ")).into_bytes();
        let mut raw = header.clone();
        raw.extend_from_slice(&bodies);
        let comp = deflate(&raw);

        let mut out = b"%PDF-1.5\n".to_vec();
        let objstm_at = out.len();
        write!(
            out,
            "{objstm_num} 0 obj\n<< /Type /ObjStm /N {} /First {} /Length {} /Filter /FlateDecode >>\nstream\n",
            packed.len(),
            header.len(),
            comp.len()
        )
        .expect("write to Vec");
        out.extend_from_slice(&comp);
        out.extend_from_slice(b"\nendstream\nendobj\n");

        let xref_at = out.len();
        let index: std::collections::HashMap<usize, usize> = packed
            .iter()
            .enumerate()
            .map(|(i, (n, _))| (*n, i))
            .collect();
        let mut rows: Vec<u8> = Vec::new();
        let mut row = |kind: u8, a: u32, b: u16| {
            rows.push(kind);
            rows.extend_from_slice(&a.to_be_bytes());
            rows.extend_from_slice(&b.to_be_bytes());
        };
        row(0, 0, 65535);
        for n in 1..top {
            if n == objstm_num {
                row(1, objstm_at as u32, 0);
            } else if n == xref_num {
                row(1, xref_at as u32, 0);
            } else if let Some(i) = index.get(&n) {
                row(2, objstm_num as u32, *i as u16);
            } else {
                row(0, 0, 65535);
            }
        }
        let rows_comp = deflate(&rows);
        write!(
            out,
            "{xref_num} 0 obj\n<< /Type /XRef /Size {top} /W [1 4 2] /Root 1 0 R /Filter /FlateDecode /Length {} >>\nstream\n",
            rows_comp.len()
        )
        .expect("write to Vec");
        out.extend_from_slice(&rows_comp);
        out.extend_from_slice(b"\nendstream\nendobj\n");
        write!(out, "startxref\n{xref_at}\n%%EOF\n").expect("write to Vec");
        out
    }

    fn deflate(raw: &[u8]) -> Vec<u8> {
        use std::io::Write;

        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(raw).expect("write to Vec");
        enc.finish().expect("finish zlib stream")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_probe::fixture;

    #[test]
    fn classic_xref_pages_are_counted() {
        for pages in [1, 8, 13, 50] {
            assert_eq!(pdf_page_count(&fixture::classic(pages)), Some(pages as u32));
        }
    }

    /// The reason a real parser is mandatory: these documents contain no
    /// `/Type /Page` bytes at all, so the scan a reviewer would reach for
    /// first reports zero pages for a 200-page file.
    #[test]
    fn object_stream_pages_are_counted_though_no_page_object_is_visible() {
        for pages in [1, 13, 64, 200] {
            let bytes = fixture::object_stream(pages);
            assert_eq!(
                bytes
                    .windows(b"/Type /Page".len())
                    .filter(|w| *w == b"/Type /Page")
                    .count(),
                0,
                "{pages}: fixture is not actually hiding its page objects"
            );
            assert_eq!(pdf_page_count(&bytes), Some(pages as u32));
        }
    }

    /// The reduction `Document::get_pages` applies silently while the
    /// caller reads its answer as an upper bound. This document declares
    /// 40 pages and hands the walker `/Kids` it cannot resolve, so the
    /// walk returns a number small enough to pass the 12-page gate — the
    /// document is then BOTH priced at 39,000 and delivered, while the
    /// provider bills 312,000. The declared `/Count` is what makes the
    /// answer conservative again.
    #[test]
    fn a_page_tree_the_walker_cannot_follow_is_bounded_by_the_declared_count() {
        const DECLARED: u32 = 40;
        const WALKABLE: usize = 5;
        let bytes = fixture::classic_understating_walk(WALKABLE, DECLARED, 40);

        let walked = lopdf::Document::load_mem(&bytes)
            .expect("fixture loads")
            .get_pages()
            .len() as u32;
        assert_eq!(walked, WALKABLE as u32, "fixture is not losing pages");
        assert!(
            walked <= crate::MAX_PDF_PAGES && DECLARED > crate::MAX_PDF_PAGES,
            "the walk must PASS the gate the real count fails, got {walked}"
        );
        assert_eq!(pdf_page_count(&bytes), Some(DECLARED));
    }

    /// The other direction, so the maximum can never be re-read as
    /// "declared wins": a real page tree under a `/Count` of zero prices
    /// from the walk.
    #[test]
    fn a_document_whose_declared_count_lies_low_still_prices_from_the_walk() {
        for pages in [1usize, 7, 30] {
            let bytes = fixture::classic_understating_count(pages);
            assert_eq!(
                lopdf::Document::load_metadata_mem(&bytes)
                    .expect("fixture loads")
                    .page_count,
                0,
                "fixture is not actually declaring zero"
            );
            assert_eq!(pdf_page_count(&bytes), Some(pages as u32));
        }
    }

    /// A provider tiles an image by its PIXEL grid, so the dimensions are
    /// the price and a 5 MiB payload cap bounds nothing: the 12000x9000
    /// case below fits in well under a megabyte of flat PNG and costs
    /// 49,536 tokens.
    #[test]
    fn image_dimensions_are_read_from_the_header() {
        for (w, h) in [
            (1u32, 1u32),
            (3024, 4032),
            (4096, 4096),
            (8064, 6048),
            (12000, 9000),
            (1170, 23400),
        ] {
            assert_eq!(image_dimensions(&fixture::png(w, h)), Some((w, h)));
        }
        assert_eq!(image_dimensions(b""), None);
        assert_eq!(image_dimensions(b"not an image"), None);
        // A vector image has no pixel grid to read, which is why delivery
        // refuses it rather than pricing it.
        assert_eq!(
            image_dimensions(br#"<svg width="100000" height="100000"/>"#),
            None
        );
        let png = fixture::png(4096, 4096);
        assert_eq!(image_dimensions(&png[..8]), None);
    }

    #[test]
    fn non_pdf_and_damaged_input_yield_none() {
        assert_eq!(pdf_page_count(b""), None);
        assert_eq!(pdf_page_count(b"not a pdf at all"), None);
        assert_eq!(pdf_page_count(&vec![0xFF; 4096]), None);
        let whole = fixture::object_stream(13);
        assert_eq!(pdf_page_count(&whole[..whole.len() / 2]), None);
        assert_eq!(pdf_page_count(&whole[..32]), None);
    }

    /// Cross-checked against `ffmpeg`-produced fixtures of a known
    /// 137,000 ms: opus/ogg 137,000, flac 137,000, wav 137,000, aac/m4a
    /// 137,023, mp3 CBR 137,038, mp3 VBR 137,038 — every container within
    /// 38 ms, VBR included (lofty reads the Xing header rather than doing
    /// header math). The in-tree fixture is WAV because it is the only
    /// one that can be synthesised byte-exactly without an encoder.
    #[test]
    fn audio_duration_is_read_from_the_container() {
        for seconds in [1, 60, 137] {
            assert_eq!(
                audio_duration_ms(&fixture::wav(seconds)),
                Some(seconds * 1_000)
            );
        }
        assert_eq!(audio_duration_ms(b""), None);
        assert_eq!(audio_duration_ms(b"not audio"), None);
        let wav = fixture::wav(60);
        assert_eq!(audio_duration_ms(&wav[..8]), None);
    }
}
