//! An `.ico` whose images are the PNG files themselves.
//!
//! Vista and later read a PNG stored directly in an icon resource, so the
//! build does not decode pixels. A width or height of 256 does not fit in the
//! directory's one byte; the format stores that size as zero and the PNG's
//! own header carries the real dimension.

use std::io::{self, Write};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

pub fn write_ico(out: &mut impl Write, images: &[(u32, &[u8])]) -> io::Result<()> {
    let count = u16::try_from(images.len())
        .map_err(|_| invalid("an icon resource holds at most 65535 images"))?;

    let mut directory = Vec::with_capacity(6 + images.len() * 16);
    directory.extend(0u16.to_le_bytes());
    directory.extend(1u16.to_le_bytes());
    directory.extend(count.to_le_bytes());

    let mut offset = 6u32 + u32::from(count) * 16;
    for (size, png) in images {
        if png.len() < 24 || !png.starts_with(PNG_SIGNATURE) {
            return Err(invalid("icon image is not a png"));
        }
        let dim = if *size >= 256 {
            0
        } else {
            u8::try_from(*size).map_err(|_| invalid("icon size does not fit one byte"))?
        };
        directory.push(dim);
        directory.push(dim);
        directory.push(0);
        directory.push(0);
        directory.extend(1u16.to_le_bytes());
        directory.extend(32u16.to_le_bytes());
        let len =
            u32::try_from(png.len()).map_err(|_| invalid("png does not fit an icon entry"))?;
        directory.extend(len.to_le_bytes());
        directory.extend(offset.to_le_bytes());
        offset = offset
            .checked_add(len)
            .ok_or_else(|| invalid("icon resource is too large"))?;
    }

    out.write_all(&directory)?;
    for (_, png) in images {
        out.write_all(png)?;
    }
    Ok(())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_png_is_stored_whole_and_256_is_a_zero_dimension() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/linux/icons");
        let sizes = [16u32, 32, 64, 128, 256];
        let loaded = sizes.map(|size| {
            let bytes = std::fs::read(root.join(format!("dbdelve-{size}.png")))
                .unwrap_or_else(|error| panic!("dbdelve-{size}.png: {error}"));
            (size, bytes)
        });
        let images = loaded
            .iter()
            .map(|(size, bytes)| (*size, bytes.as_slice()))
            .collect::<Vec<_>>();

        let mut ico = Vec::new();
        write_ico(&mut ico, &images).expect("the icon resource must build");

        assert_eq!(&ico[0..2], &0u16.to_le_bytes());
        assert_eq!(&ico[2..4], &1u16.to_le_bytes());
        assert_eq!(&ico[4..6], &(sizes.len() as u16).to_le_bytes());

        for (index, (size, png)) in images.iter().enumerate() {
            let entry = 6 + index * 16;
            let dim = if *size >= 256 { 0 } else { *size as u8 };
            assert_eq!(ico[entry], dim, "width byte for {size}");
            assert_eq!(ico[entry + 1], dim, "height byte for {size}");
            let len = u32::from_le_bytes(ico[entry + 8..entry + 12].try_into().unwrap()) as usize;
            let offset =
                u32::from_le_bytes(ico[entry + 12..entry + 16].try_into().unwrap()) as usize;
            assert_eq!(len, png.len());
            assert_eq!(&ico[offset..offset + png.len()], *png);
            let declared = u32::from_be_bytes(png[16..20].try_into().unwrap());
            assert_eq!(declared, *size, "png header does not match its filename");
        }
    }
}
