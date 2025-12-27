#!/usr/bin/env python3
"""Generate icon_data.rs from icon.png using pure Python PNG decoding."""

import zlib
import struct
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
ICON_PATH = PROJECT_ROOT / "assets" / "icon.png"
OUTPUT_PATH = PROJECT_ROOT / "src" / "icon_data.rs"


def decode_png(data: bytes) -> tuple[int, int, bytes]:
    """Minimal PNG decoder - returns (width, height, rgba_bytes)."""
    if data[:8] != b'\x89PNG\r\n\x1a\n':
        raise ValueError("Not a valid PNG file")

    pos = 8
    width = height = bit_depth = color_type = 0
    idat_chunks = []

    while pos < len(data):
        length = struct.unpack(">I", data[pos:pos+4])[0]
        chunk_type = data[pos+4:pos+8]
        chunk_data = data[pos+8:pos+8+length]
        pos += 12 + length  # 4 (len) + 4 (type) + length + 4 (crc)

        if chunk_type == b'IHDR':
            width, height, bit_depth, color_type = struct.unpack(">IIBB", chunk_data[:10])
        elif chunk_type == b'IDAT':
            idat_chunks.append(chunk_data)
        elif chunk_type == b'IEND':
            break

    # Decompress image data
    compressed = b''.join(idat_chunks)
    decompressed = zlib.decompress(compressed)

    # Determine bytes per pixel (at source bit depth)
    channels = 3 if color_type == 2 else 4  # RGB or RGBA
    bpp = channels * (bit_depth // 8)
    stride = width * bpp + 1  # +1 for filter byte

    # Unfilter all rows first (at original bit depth)
    rows = []
    prev_row = bytearray(width * bpp)

    for y in range(height):
        row_start = y * stride
        filter_type = decompressed[row_start]
        row_data = bytearray(decompressed[row_start+1:row_start+stride])

        # Apply PNG filters
        if filter_type == 1:  # Sub
            for i in range(bpp, len(row_data)):
                row_data[i] = (row_data[i] + row_data[i - bpp]) & 0xff
        elif filter_type == 2:  # Up
            for i in range(len(row_data)):
                row_data[i] = (row_data[i] + prev_row[i]) & 0xff
        elif filter_type == 3:  # Average
            for i in range(len(row_data)):
                left = row_data[i - bpp] if i >= bpp else 0
                up = prev_row[i]
                row_data[i] = (row_data[i] + (left + up) // 2) & 0xff
        elif filter_type == 4:  # Paeth
            for i in range(len(row_data)):
                left = row_data[i - bpp] if i >= bpp else 0
                up = prev_row[i]
                up_left = prev_row[i - bpp] if i >= bpp else 0
                p = left + up - up_left
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - up_left)
                if pa <= pb and pa <= pc:
                    pred = left
                elif pb <= pc:
                    pred = up
                else:
                    pred = up_left
                row_data[i] = (row_data[i] + pred) & 0xff

        rows.append(bytes(row_data))
        prev_row = row_data

    # Now convert to 8-bit RGBA
    rgba = bytearray()
    for row in rows:
        if bit_depth == 16:
            # Take high byte of each 16-bit value (big-endian)
            row = bytes(row[i] for i in range(0, len(row), 2))

        if color_type == 2:  # RGB -> RGBA
            for i in range(0, len(row), 3):
                rgba.extend(row[i:i+3])
                rgba.append(255)
        else:  # Already RGBA
            rgba.extend(row)

    return width, height, bytes(rgba)


def main():
    print(f"Reading {ICON_PATH}...")
    png_data = ICON_PATH.read_bytes()

    width, height, rgba = decode_png(png_data)
    print(f"Decoded: {width}x{height}, {len(rgba)} bytes RGBA")

    # Generate Rust source
    hex_bytes = ', '.join(f'0x{b:02x}' for b in rgba)
    rust_code = f"""// Auto-generated from assets/icon.png - do not edit
// Icon: {width}x{height} RGBA ({len(rgba)} bytes)

pub const ICON_WIDTH: u32 = {width};
pub const ICON_HEIGHT: u32 = {height};
pub const ICON_RGBA: &[u8] = &[{hex_bytes}];
"""

    OUTPUT_PATH.write_text(rust_code)
    print(f"Generated {OUTPUT_PATH} ({len(rust_code)} bytes)")


if __name__ == "__main__":
    main()
