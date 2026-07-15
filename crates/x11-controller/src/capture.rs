use std::io::Cursor;

use x11rb::protocol::xproto::ImageOrder;

use crate::{ControllerError, ErrorCode, Result};

#[derive(Debug, Clone, Copy)]
pub(crate) struct PixelFormat {
    pub bits_per_pixel: u8,
    pub scanline_pad: u8,
    pub byte_order: ImageOrder,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

pub(crate) fn convert_to_rgb(
    data: &[u8],
    width: u32,
    height: u32,
    format: PixelFormat,
) -> Result<Vec<u8>> {
    if width == 0 || height == 0 {
        return Err(ControllerError::new(
            ErrorCode::InvalidInput,
            "capture dimensions must be non-zero",
        ));
    }
    let bytes_per_pixel = usize::from(format.bits_per_pixel.div_ceil(8));
    if !matches!(bytes_per_pixel, 1..=4) {
        return Err(ControllerError::new(
            ErrorCode::UnsupportedCapability,
            format!(
                "unsupported X11 pixel width: {} bits",
                format.bits_per_pixel
            ),
        ));
    }
    let row_bits = usize::try_from(width)
        .map_err(|_| ControllerError::new(ErrorCode::InvalidInput, "capture is too wide"))?
        .checked_mul(usize::from(format.bits_per_pixel))
        .ok_or_else(|| ControllerError::new(ErrorCode::InvalidInput, "capture is too large"))?;
    let pad = usize::from(format.scanline_pad.max(8));
    let stride = row_bits
        .div_ceil(pad)
        .checked_mul(pad / 8)
        .ok_or_else(|| ControllerError::new(ErrorCode::InvalidInput, "capture is too large"))?;
    let expected = stride
        .checked_mul(usize::try_from(height).unwrap_or(usize::MAX))
        .ok_or_else(|| ControllerError::new(ErrorCode::InvalidInput, "capture is too large"))?;
    if data.len() < expected {
        return Err(ControllerError::new(
            ErrorCode::X11,
            format!(
                "short GetImage reply: expected at least {expected} bytes, received {}",
                data.len()
            ),
        ));
    }

    let pixel_count = usize::try_from(width)
        .unwrap_or(usize::MAX)
        .checked_mul(usize::try_from(height).unwrap_or(usize::MAX))
        .ok_or_else(|| ControllerError::new(ErrorCode::InvalidInput, "capture is too large"))?;
    let mut rgb = Vec::with_capacity(pixel_count.saturating_mul(3));
    for y in 0..usize::try_from(height).unwrap_or(0) {
        let row = &data[y * stride..(y + 1) * stride];
        for x in 0..usize::try_from(width).unwrap_or(0) {
            let offset = x * bytes_per_pixel;
            let bytes = &row[offset..offset + bytes_per_pixel];
            let pixel = match format.byte_order {
                ImageOrder::LSB_FIRST => bytes
                    .iter()
                    .enumerate()
                    .fold(0_u32, |value, (shift, byte)| {
                        value | (u32::from(*byte) << (shift * 8))
                    }),
                ImageOrder::MSB_FIRST => bytes
                    .iter()
                    .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte)),
                _ => {
                    return Err(ControllerError::new(
                        ErrorCode::UnsupportedCapability,
                        "unknown X11 image byte order",
                    ));
                }
            };
            rgb.push(scale_mask(pixel, format.red_mask));
            rgb.push(scale_mask(pixel, format.green_mask));
            rgb.push(scale_mask(pixel, format.blue_mask));
        }
    }
    Ok(rgb)
}

fn scale_mask(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shifted = (pixel & mask) >> mask.trailing_zeros();
    let max = mask >> mask.trailing_zeros();
    u8::try_from((u64::from(shifted) * 255 + u64::from(max / 2)) / u64::from(max)).unwrap_or(255)
}

pub(crate) fn encode_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(Cursor::new(&mut output), width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| ControllerError::new(ErrorCode::Internal, error.to_string()))?;
        writer
            .write_image_data(rgb)
            .map_err(|error| ControllerError::new(ErrorCode::Internal, error.to_string()))?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_little_endian_bgrx() {
        let rgb = convert_to_rgb(
            &[0x33, 0x22, 0x11, 0],
            1,
            1,
            PixelFormat {
                bits_per_pixel: 32,
                scanline_pad: 32,
                byte_order: ImageOrder::LSB_FIRST,
                red_mask: 0x00ff_0000,
                green_mask: 0x0000_ff00,
                blue_mask: 0x0000_00ff,
            },
        )
        .expect("valid pixel");
        assert_eq!(rgb, [0x11, 0x22, 0x33]);
    }

    #[test]
    fn converts_big_endian_rgb565() {
        let rgb = convert_to_rgb(
            &[0xf8, 0x00],
            1,
            1,
            PixelFormat {
                bits_per_pixel: 16,
                scanline_pad: 16,
                byte_order: ImageOrder::MSB_FIRST,
                red_mask: 0xf800,
                green_mask: 0x07e0,
                blue_mask: 0x001f,
            },
        )
        .expect("valid pixel");
        assert_eq!(rgb, [255, 0, 0]);
    }
}
