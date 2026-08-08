//! Getting the configuration XML out of what `/dev/fsget` hands over.
//!
//! Two layouts occur in `/prog` and are told apart by their magic bytes.
//! `sps_<ver>_<ts>.zip` is the deployment archive holding `sps0.LoxCC`, while
//! `sps_<ver>_<ts>.LoxCC` (written by the Miniserver since firmware 17) is a
//! bare LoxCC container. Both yield the same XML.
//!
//! A LoxCC container is a 16-byte header - magic, compressed size, uncompressed
//! size, CRC32 - followed by an LZ4 payload. Every one of those four fields is
//! checked, because the thing being validated is a download: a truncated or
//! half-written file is a far likelier failure here than a corrupt one, and both
//! have to be told apart from a Miniserver that answered with an error page.

use std::borrow::Cow;

use log::debug;

use super::SyncError;

const LOXCC_MAGIC: u32 = 0xaabb_ccee;
const ZIP_MAGIC: &[u8; 2] = b"PK";
/// The entry a deployment archive keeps the configuration in.
const ZIP_ENTRY: &[u8] = b"sps0.LoxCC";

/// The configuration XML, out of whichever wrapper it arrived in.
pub(crate) fn decompress(payload: &[u8]) -> Result<Vec<u8>, SyncError> {
    if payload.starts_with(ZIP_MAGIC) {
        let entry = zip_entry(payload, ZIP_ENTRY)?;
        return read_container(&entry);
    }
    if payload.len() >= 4 && u32::from_le_bytes(payload[..4].try_into().unwrap()) == LOXCC_MAGIC {
        return read_container(payload);
    }
    Err(SyncError::UnexpectedPayload {
        len: payload.len(),
        head: payload[..payload.len().min(32)].to_vec(),
    })
}

/// Check a LoxCC container and decompress it.
fn read_container(container: &[u8]) -> Result<Vec<u8>, SyncError> {
    if container.len() < 16 || u32::from_le_bytes(container[..4].try_into().unwrap()) != LOXCC_MAGIC
    {
        return Err(SyncError::Container("Invalid file format".to_owned()));
    }
    let compressed_size = u32::from_le_bytes(container[4..8].try_into().unwrap()) as usize;
    let uncompressed_size = u32::from_le_bytes(container[8..12].try_into().unwrap()) as usize;
    let checksum = u32::from_le_bytes(container[12..16].try_into().unwrap());

    let payload = &container[16..];
    // The bytes are already in hand, so the check is that there are enough of
    // them. It is what catches a truncated download.
    if payload.len() < compressed_size {
        return Err(SyncError::Container(format!(
            "Payload length mismatch: got {}, expected {compressed_size}",
            payload.len()
        )));
    }
    let payload = &payload[..compressed_size];

    debug!("Using LZ4 decompression");
    let result = decompress_lz4(payload, uncompressed_size)?;

    if checksum != crc32fast::hash(&result) {
        return Err(SyncError::Container(
            "Checksum verification failed".to_owned(),
        ));
    }
    if result.len() != uncompressed_size {
        return Err(SyncError::Container(format!(
            "Uncompressed filesize mismatch: {} != {uncompressed_size}",
            result.len()
        )));
    }
    Ok(result)
}

/// Whether these bytes open an LZ4 *frame* rather than a bare block.
///
/// Loxone writes blocks, but the detection costs four bytes, so a firmware that
/// ever switches does not need a code change.
fn is_lz4_frame(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let magic = u32::from_le_bytes(data[..4].try_into().unwrap());
    matches!(magic, 0x184D_2204 | 0x184C_2102) || (0x184D_2A50..=0x184D_2A5F).contains(&magic)
}

/// Decompress an LZ4 payload of either flavour.
fn decompress_lz4(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>, SyncError> {
    if is_lz4_frame(data) {
        return decompress_frame(data);
    }
    match decompress_block(data, uncompressed_size) {
        Ok(out) => Ok(out),
        // Possibly misidentified - try the other way round before giving up,
        // exactly as `_decompress_loxcc_block_lz4` did.
        Err(block_error) => decompress_frame(data).map_err(|_| block_error),
    }
}

/// LZ4 block mode, with the size the header promised as the *upper bound*.
///
/// `lz4_flex::block::decompress` writes through an unchecked pointer into a
/// `Vec::with_capacity`, and its own documentation says it may panic when the
/// size passed is smaller than the real output. A header that understates its
/// uncompressed size is exactly what a corrupt download looks like, so that
/// would turn bad input into a dead process. `decompress_into` writes into a
/// bounded slice instead, which is what `LZ4_decompress_safe` does.
fn decompress_block(data: &[u8], uncompressed_size: usize) -> Result<Vec<u8>, SyncError> {
    let mut out = vec![0u8; uncompressed_size];
    match lz4_flex::block::decompress_into(data, &mut out) {
        Ok(written) => {
            // A short result is not an error here: the size check in
            // `read_container` is what reports it, with both numbers.
            out.truncate(written);
            Ok(out)
        }
        Err(e) => Err(SyncError::Container(format!(
            "LZ4 decompression failed: {e}"
        ))),
    }
}

fn decompress_frame(data: &[u8]) -> Result<Vec<u8>, SyncError> {
    use std::io::Read as _;

    let mut out = Vec::new();
    lz4_flex::frame::FrameDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| SyncError::Container(format!("LZ4 decompression failed: {e}")))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// The zip case
// ---------------------------------------------------------------------------

/// One named entry out of a zip archive.
///
/// Hand-rolled rather than taken from the `zip` crate, which by default carries
/// aes, bzip2, zstd, lzma and zopfli for what is here a single fixed name in a
/// ~400 KB archive written by a Miniserver.
///
/// Sizes and the compression method come from the central directory, never from
/// the local header: the local one is allowed to leave them zero and defer to a
/// data descriptor. The *offset* of the data, on the other hand, has to be
/// recomputed from the local header's own name and extra lengths, which may
/// legitimately differ from the central copy.
fn zip_entry<'a>(archive: &'a [u8], name: &[u8]) -> Result<Cow<'a, [u8]>, SyncError> {
    let eocd = find_eocd(archive)?;
    let entries = u16::from_le_bytes(archive[eocd + 10..eocd + 12].try_into().unwrap());
    let directory = u32::from_le_bytes(archive[eocd + 16..eocd + 20].try_into().unwrap()) as usize;

    if entries == u16::MAX || directory == u32::MAX as usize {
        return Err(SyncError::Archive(
            "the archive is ZIP64, which this reader does not handle".to_owned(),
        ));
    }

    let mut at = directory;
    for _ in 0..entries {
        if archive.len() < at + 46 || &archive[at..at + 4] != b"PK\x01\x02" {
            return Err(SyncError::Archive(
                "the central directory is malformed".to_owned(),
            ));
        }
        let method = u16::from_le_bytes(archive[at + 10..at + 12].try_into().unwrap());
        let compressed = u32::from_le_bytes(archive[at + 20..at + 24].try_into().unwrap()) as usize;
        let uncompressed =
            u32::from_le_bytes(archive[at + 24..at + 28].try_into().unwrap()) as usize;
        let name_len = u16::from_le_bytes(archive[at + 28..at + 30].try_into().unwrap()) as usize;
        let extra_len = u16::from_le_bytes(archive[at + 30..at + 32].try_into().unwrap()) as usize;
        let comment_len = u16::from_le_bytes(archive[at + 32..at + 34].try_into().unwrap()) as usize;
        let local = u32::from_le_bytes(archive[at + 42..at + 46].try_into().unwrap()) as usize;

        let entry_name = archive.get(at + 46..at + 46 + name_len).ok_or_else(|| {
            SyncError::Archive("the central directory is truncated".to_owned())
        })?;
        if entry_name == name {
            return read_local(archive, local, method, compressed, uncompressed);
        }
        at += 46 + name_len + extra_len + comment_len;
    }

    Err(SyncError::Archive(format!(
        "the archive has no entry named {}",
        String::from_utf8_lossy(name)
    )))
}

fn read_local<'a>(
    archive: &'a [u8],
    at: usize,
    method: u16,
    compressed: usize,
    uncompressed: usize,
) -> Result<Cow<'a, [u8]>, SyncError> {
    if archive.len() < at + 30 || &archive[at..at + 4] != b"PK\x03\x04" {
        return Err(SyncError::Archive(
            "the local file header is malformed".to_owned(),
        ));
    }
    let name_len = u16::from_le_bytes(archive[at + 26..at + 28].try_into().unwrap()) as usize;
    let extra_len = u16::from_le_bytes(archive[at + 28..at + 30].try_into().unwrap()) as usize;
    let start = at + 30 + name_len + extra_len;
    let data = archive
        .get(start..start + compressed)
        .ok_or_else(|| SyncError::Archive("the entry is truncated".to_owned()))?;

    match method {
        0 => Ok(Cow::Borrowed(data)),
        8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, uncompressed)
            .map(Cow::Owned)
            .map_err(|e| SyncError::Archive(format!("the entry could not be inflated: {e:?}"))),
        other => Err(SyncError::Archive(format!(
            "the entry uses compression method {other}, which this reader does not handle"
        ))),
    }
}

/// The end-of-central-directory record, searched from the tail.
fn find_eocd(archive: &[u8]) -> Result<usize, SyncError> {
    // 22 bytes of record plus a comment of at most 65535.
    let window = archive.len().min(22 + u16::MAX as usize);
    let start = archive.len() - window;
    archive[start..]
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .map(|at| start + at)
        .filter(|at| archive.len() >= at + 22)
        .ok_or_else(|| SyncError::Archive("the archive has no end-of-directory record".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap `xml` in a LoxCC container with an LZ4 block payload.
    fn container(xml: &[u8]) -> Vec<u8> {
        let compressed = lz4_flex::block::compress(xml);
        let mut out = Vec::new();
        out.extend_from_slice(&LOXCC_MAGIC.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc32fast::hash(xml).to_le_bytes());
        out.extend_from_slice(&compressed);
        out
    }

    /// The same, with an LZ4 *frame* payload.
    fn container_framed(xml: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        encoder.write_all(xml).expect("encode");
        let compressed = encoder.finish().expect("finish");
        let mut out = Vec::new();
        out.extend_from_slice(&LOXCC_MAGIC.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc32fast::hash(xml).to_le_bytes());
        out.extend_from_slice(&compressed);
        out
    }

    /// A single-entry archive, stored or deflated.
    fn archive(name: &[u8], body: &[u8], deflate: bool, extra_local: usize) -> Vec<u8> {
        let stored: Vec<u8> = if deflate {
            miniz_oxide::deflate::compress_to_vec(body, 6)
        } else {
            body.to_vec()
        };
        let method: u16 = if deflate { 8 } else { 0 };
        let crc = crc32fast::hash(body);

        let mut out = Vec::new();
        let local_at = out.len();
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&[20, 0]); // version needed
        out.extend_from_slice(&[0, 0]); // flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]); // time, date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(extra_local as u16).to_le_bytes());
        out.extend_from_slice(name);
        out.extend(std::iter::repeat_n(0u8, extra_local));
        out.extend_from_slice(&stored);

        let dir_at = out.len();
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&[20, 0, 20, 0]); // version made by / needed
        out.extend_from_slice(&[0, 0]); // flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]); // time, date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]); // extra length, deliberately not the local one
        out.extend_from_slice(&[0, 0]); // comment length
        out.extend_from_slice(&[0, 0, 0, 0]); // disk, internal attrs
        out.extend_from_slice(&[0, 0, 0, 0]); // external attrs
        out.extend_from_slice(&(local_at as u32).to_le_bytes());
        out.extend_from_slice(name);

        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0, 0, 0, 0]); // disk numbers
        out.extend_from_slice(&1u16.to_le_bytes()); // entries on this disk
        out.extend_from_slice(&1u16.to_le_bytes()); // entries total
        out.extend_from_slice(&((out.len() - dir_at) as u32).to_le_bytes());
        out.extend_from_slice(&(dir_at as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0]); // comment length
        out
    }

    const XML: &[u8] = b"<ControlList><C Type=\"VirtualInCaption\"><C Title=\"x\"/></C></ControlList>";

    #[test]
    fn a_bare_container_round_trips() {
        assert_eq!(decompress(&container(XML)).unwrap(), XML);
    }

    #[test]
    fn a_frame_encoded_payload_is_detected() {
        assert_eq!(decompress(&container_framed(XML)).unwrap(), XML);
    }

    #[test]
    fn a_zip_yields_the_same_bytes_as_a_bare_container() {
        let stored = archive(ZIP_ENTRY, &container(XML), false, 0);
        assert_eq!(decompress(&stored).unwrap(), XML);
        let deflated = archive(ZIP_ENTRY, &container(XML), true, 0);
        assert_eq!(decompress(&deflated).unwrap(), XML);
    }

    /// The local header may carry an extra field the central one does not, so
    /// the data offset has to come from the local header itself.
    #[test]
    fn a_local_header_with_its_own_extra_field_is_handled() {
        let with_extra = archive(ZIP_ENTRY, &container(XML), false, 17);
        assert_eq!(decompress(&with_extra).unwrap(), XML);
    }

    #[test]
    fn a_missing_entry_is_reported() {
        let wrong = archive(b"something-else", &container(XML), false, 0);
        let error = decompress(&wrong).unwrap_err().to_string();
        assert!(error.contains("no entry named sps0.LoxCC"), "{error}");
    }

    #[test]
    fn the_frame_magics_are_recognised() {
        assert!(is_lz4_frame(&0x184D_2204u32.to_le_bytes()));
        assert!(is_lz4_frame(&0x184C_2102u32.to_le_bytes()));
        for magic in 0x184D_2A50u32..=0x184D_2A5F {
            assert!(is_lz4_frame(&magic.to_le_bytes()), "{magic:#x}");
        }
        // The boundaries either side, and a block payload.
        assert!(!is_lz4_frame(&0x184D_2A4Fu32.to_le_bytes()));
        assert!(!is_lz4_frame(&0x184D_2A60u32.to_le_bytes()));
        assert!(!is_lz4_frame(&[0xf4, 0x00, 0x00]));
    }

    #[test]
    fn a_wrong_magic_is_rejected() {
        let mut bad = container(XML);
        bad[0] ^= 0xff;
        // No longer a LoxCC and not a zip either, so it does not even get as far
        // as the header check.
        assert!(matches!(
            decompress(&bad),
            Err(SyncError::UnexpectedPayload { .. })
        ));

        // Inside a zip, the container check is the one that fires.
        let mut inner = container(XML);
        inner[0] ^= 0xff;
        let wrapped = archive(ZIP_ENTRY, &inner, false, 0);
        assert_eq!(
            decompress(&wrapped).unwrap_err().to_string(),
            "Invalid file format"
        );
    }

    #[test]
    fn a_truncated_payload_is_reported_with_both_numbers() {
        let full = container(XML);
        let cut = &full[..full.len() - 5];
        let error = decompress(cut).unwrap_err().to_string();
        assert!(error.starts_with("Payload length mismatch: got "), "{error}");
    }

    #[test]
    fn a_corrupted_payload_fails_its_checksum() {
        let mut bad = container(XML);
        // Flip a bit in the recorded CRC rather than in the payload, so the
        // decompression still succeeds and the checksum is what catches it.
        bad[12] ^= 0x01;
        assert_eq!(
            decompress(&bad).unwrap_err().to_string(),
            "Checksum verification failed"
        );
    }

    /// A header claiming less than the payload really holds must be an error and
    /// must not take the process down.
    ///
    /// This is the regression test for using `decompress_into` over
    /// `lz4_flex::block::decompress`, whose own docs say it may panic here.
    #[test]
    fn a_lying_uncompressed_size_is_rejected_rather_than_fatal() {
        let mut bad = container(XML);
        bad[8..12].copy_from_slice(&4u32.to_le_bytes());
        let error = decompress(&bad).unwrap_err().to_string();
        assert!(
            error.contains("LZ4 decompression failed")
                || error.contains("Uncompressed filesize mismatch")
                || error.contains("Checksum verification failed"),
            "{error}"
        );
    }

    /// The Miniserver answers some requests with a JSON error body and status
    /// 200, so "not a configuration" has to be told apart from "corrupt".
    #[test]
    fn an_error_body_is_reported_as_an_unexpected_payload() {
        let body = br#"{"LL":{"control":"dev/fsget","Code":"403"}}"#;
        let error = decompress(body).unwrap_err().to_string();
        assert_eq!(
            error,
            format!(
                "Unexpected configuration payload: {} bytes starting with \
                 {{\"LL\":{{\"control\":\"dev/fsget\",\"Co",
                body.len()
            ),
            "the head is the first 32 bytes, as loggable_bytes gives them"
        );
    }

    #[test]
    fn an_empty_body_is_reported_rather_than_indexed_into() {
        assert!(decompress(b"").is_err());
        assert!(decompress(b"PK").is_err());
    }
}
