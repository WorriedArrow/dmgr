//! Chunk codecs.

use std::io::{Read, Write};

use super::format::{Format, chunk};
use crate::error::{Error, Result, corrupt};

/// Compresses one chunk with the codec of `format`.
pub(crate) fn compress(format: Format, level: u32, data: &[u8]) -> Result<Vec<u8>> {
    match format {
        Format::Udrw | Format::Udro => Ok(data.to_vec()),
        Format::Udzo => {
            let mut e = flate2::write::ZlibEncoder::new(
                Vec::with_capacity(data.len() / 2),
                flate2::Compression::new(level.min(9)),
            );
            e.write_all(data)?;
            Ok(e.finish()?)
        }
        #[cfg(feature = "bzip2")]
        Format::Udbz => {
            let mut e = bzip2::write::BzEncoder::new(
                Vec::with_capacity(data.len() / 2),
                bzip2::Compression::new(level.clamp(1, 9)),
            );
            e.write_all(data)?;
            Ok(e.finish()?)
        }
        #[cfg(feature = "lzfse")]
        Format::Ulfo => {
            let mut out = Vec::with_capacity(data.len() / 2);
            lzfse_rust::encode_bytes(data, &mut out)?;
            Ok(out)
        }
        #[cfg(feature = "lzma")]
        Format::Ulmo => {
            let opts = lzma_rust2::XzOptions::with_preset(level.min(9));
            let mut w = lzma_rust2::XzWriter::new(Vec::with_capacity(data.len() / 2), opts)?;
            w.write_all(data)?;
            Ok(w.finish()?)
        }
        #[allow(unreachable_patterns)]
        other => Err(Error::Unsupported(format!(
            "{other} support was not compiled into this build of dmgr"
        ))),
    }
}

/// Decompresses one chunk of the given type into exactly `out_len` bytes.
pub(crate) fn decompress(kind: u32, data: &[u8], out_len: usize) -> Result<Vec<u8>> {
    let out = match kind {
        chunk::ZERO_FILL | chunk::IGNORE => return Ok(vec![0; out_len]),
        chunk::RAW => data.to_vec(),
        chunk::ADC => adc_decompress(data, out_len)?,
        chunk::ZLIB => {
            let mut out = Vec::with_capacity(out_len);
            flate2::read::ZlibDecoder::new(data).read_to_end(&mut out)?;
            out
        }
        #[cfg(feature = "bzip2")]
        chunk::BZIP2 => {
            let mut out = Vec::with_capacity(out_len);
            bzip2::read::BzDecoder::new(data).read_to_end(&mut out)?;
            out
        }
        #[cfg(feature = "lzfse")]
        chunk::LZFSE => lzfse_decode(data, out_len)?,
        #[cfg(feature = "lzma")]
        chunk::LZMA => {
            let mut out = Vec::with_capacity(out_len);
            lzma_rust2::XzReader::new(data, true).read_to_end(&mut out)?;
            out
        }
        other => {
            return Err(Error::Unsupported(format!(
                "chunk type {other:#010x} ({})",
                chunk::name(other)
            )));
        }
    };
    if out.len() < out_len {
        return Err(corrupt(format!(
            "{} chunk decompressed to {} bytes, expected {out_len}",
            chunk::name(kind),
            out.len()
        )));
    }
    let mut out = out;
    out.truncate(out_len);
    Ok(out)
}

/// Decodes an LZFSE stream (also used for decmpfs LZVN payloads).
pub(crate) fn lzfse_decode(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    #[cfg(feature = "lzfse")]
    {
        let mut out = Vec::with_capacity(expected);
        lzfse_rust::decode_bytes(data, &mut out)
            .map_err(|e| corrupt(format!("LZFSE decode failed: {e}")))?;
        Ok(out)
    }
    #[cfg(not(feature = "lzfse"))]
    {
        let _ = (data, expected);
        Err(Error::Unsupported(
            "LZFSE support was not compiled in".into(),
        ))
    }
}

/// Apple Data Compression (used by very old images).
pub(crate) fn adc_decompress(src: &[u8], out_len: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(out_len);
    let mut i = 0usize;
    let bad = || corrupt("truncated ADC data");
    while i < src.len() && out.len() < out_len {
        let b = src[i];
        if b & 0x80 != 0 {
            let len = (b & 0x7F) as usize + 1;
            let lit = src.get(i + 1..i + 1 + len).ok_or_else(bad)?;
            out.extend_from_slice(lit);
            i += 1 + len;
        } else {
            let (len, dist) = if b & 0x40 != 0 {
                let hi = *src.get(i + 1).ok_or_else(bad)? as usize;
                let lo = *src.get(i + 2).ok_or_else(bad)? as usize;
                i += 3;
                ((b & 0x3F) as usize + 4, (hi << 8) | lo)
            } else {
                let lo = *src.get(i + 1).ok_or_else(bad)? as usize;
                i += 2;
                (
                    ((b & 0x3C) >> 2) as usize + 3,
                    (((b & 0x03) as usize) << 8) | lo,
                )
            };
            let start = out
                .len()
                .checked_sub(dist + 1)
                .ok_or_else(|| corrupt("ADC back-reference before start"))?;
            for k in 0..len {
                let v = out[start + k];
                out.push(v);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..200_000u32 {
            v.extend_from_slice(&(i % 977).to_le_bytes());
        }
        v
    }

    #[test]
    fn roundtrip_all_formats() {
        let data = sample();
        for f in Format::ALL {
            if !f.is_supported() {
                continue;
            }
            let c = compress(f, f.default_level(), &data).unwrap();
            let kind = f.chunk_type().unwrap_or(chunk::RAW);
            let d = decompress(kind, &c, data.len()).unwrap();
            assert_eq!(d, data, "{f}");
            if f.chunk_type().is_some() {
                assert!(c.len() < data.len() / 2, "{f} did not compress");
            }
        }
    }

    #[test]
    fn adc() {
        // literal "abc", then copy 3 bytes from distance 3 (two-byte form)
        let src = [0x82, b'a', b'b', b'c', 0x00, 0x02];
        assert_eq!(adc_decompress(&src, 6).unwrap(), b"abcabc");
        // three-byte form: len 4, distance 0 -> repeat last byte
        let src = [0x80, b'x', 0x40, 0x00, 0x00];
        assert_eq!(adc_decompress(&src, 5).unwrap(), b"xxxxx");
    }
}
