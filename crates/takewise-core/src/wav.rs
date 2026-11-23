//! The mono channel layout that hound's WAV headers leave out.
//!
//! hound writes `WAVE_FORMAT_EXTENSIBLE` for more than 16 bits and gives its
//! one channel the mask of the first speaker, front left (0x1). ffmpeg reads
//! that as a one-sided layout (`FL`), and its AAC encoder refuses it unless
//! told `-ac 1`; editors may place it on the left. Takewise's mono files are
//! tagged front centre (0x4), the layout ffmpeg and editors read as plain
//! mono. 16-bit files use the older PCM header, which has no mask.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt as _;

/// `SPEAKER_FRONT_CENTER`: one channel with no side, read as plain mono.
pub const FRONT_CENTER: u32 = 0x4;

/// `dwChannelMask` in a `WAVE_FORMAT_EXTENSIBLE` header whose fmt chunk comes
/// first, as hound writes it: after the RIFF header (12 bytes), the fmt
/// chunk's header (8) and 20 bytes of the format.
const MASK_AT: usize = 40;

/// The channel mask of a WAV header (its first 44 bytes or more), when the
/// header is `WAVE_FORMAT_EXTENSIBLE` with the fmt chunk first.
pub fn channel_mask(header: &[u8]) -> Option<u32> {
    let bytes = |at: usize, len: usize| header.get(at..at + len);
    let extensible = bytes(0, 4) == Some(b"RIFF")
        && bytes(8, 4) == Some(b"WAVE")
        && bytes(12, 4) == Some(b"fmt ")
        && bytes(20, 2) == Some(&0xfffe_u16.to_le_bytes());
    let mask = bytes(MASK_AT, 4)?;
    extensible.then(|| u32::from_le_bytes([mask[0], mask[1], mask[2], mask[3]]))
}

fn is_extensible_mono(header: &[u8]) -> bool {
    channel_mask(header).is_some() && header.get(22..24) == Some(&1_u16.to_le_bytes())
}

/// Tags the channel of an in-memory mono WAV header as front centre. Leaves
/// any other header as it is.
pub fn centre_mono(header: &mut [u8]) {
    if is_extensible_mono(header) {
        header[MASK_AT..MASK_AT + 4].copy_from_slice(&FRONT_CENTER.to_le_bytes());
    }
}

/// Tags the channel of the mono WAV in `file` as front centre. Leaves any
/// other file as it is.
pub fn centre_mono_file(file: &File) -> io::Result<()> {
    let mut header = [0; MASK_AT + 4];
    file.read_exact_at(&mut header, 0)?;
    if is_extensible_mono(&header) {
        file.write_all_at(&FRONT_CENTER.to_le_bytes(), MASK_AT as u64)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(channels: u16, bits: u16) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels,
            sample_rate: 48_000,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        };
        let mut out = io::Cursor::new(Vec::new());
        hound::WavWriter::new(&mut out, spec)
            .and_then(hound::WavWriter::finalize)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn hound_tags_mono_front_left_and_centre_mono_fixes_it() {
        let mut mono = header(1, 24);
        assert_eq!(channel_mask(&mono), Some(0x1));
        centre_mono(&mut mono);
        assert_eq!(channel_mask(&mono), Some(FRONT_CENTER));
        // The rest of the header is hound's, so hound still reads it.
        let reader = hound::WavReader::new(io::Cursor::new(mono)).unwrap();
        assert_eq!(reader.spec().channels, 1);
    }

    #[test]
    fn other_headers_stay_as_they_are() {
        let mut stereo = header(2, 24);
        centre_mono(&mut stereo);
        assert_eq!(channel_mask(&stereo), Some(0x3));
        let sixteen = header(1, 16);
        let mut patched = sixteen.clone();
        centre_mono(&mut patched);
        assert_eq!(patched, sixteen);
        assert_eq!(channel_mask(&sixteen), None);
    }

    #[test]
    fn centre_mono_file_patches_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mono.wav");
        std::fs::write(&path, header(1, 24)).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        centre_mono_file(&file).unwrap();
        assert_eq!(
            channel_mask(&std::fs::read(&path).unwrap()),
            Some(FRONT_CENTER)
        );
    }
}
