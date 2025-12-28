#!/usr/bin/env python3
"""Create linefeed icon (PNG and ICO) with 'lf' text."""

from pathlib import Path

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError:
    print("Error: Pillow is required. Install with: apt install python3-pil")
    exit(1)

PROJECT_ROOT = Path(__file__).resolve().parent.parent
ICON_PNG = PROJECT_ROOT / "assets" / "icon.png"
ICON_ICO = PROJECT_ROOT / "assets" / "linefeed.ico"

# Icon colors
BG_COLOR = (59, 89, 152, 255)  # Blue
TEXT_COLOR = (255, 255, 255, 255)  # White
TEXT = "lf"

# Font paths to try (in order of preference)
FONT_PATHS = [
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/freefont/FreeSansBold.ttf",
    "C:\\Windows\\Fonts\\segoeui.ttf",
    "C:\\Windows\\Fonts\\arial.ttf",
]


def get_font(size: int):
    """Get a font at the specified size, trying various system fonts."""
    for path in FONT_PATHS:
        try:
            return ImageFont.truetype(path, size)
        except (OSError, IOError):
            continue
    return ImageFont.load_default()


def create_icon(size: int) -> Image.Image:
    """Create a single icon at the specified size."""
    img = Image.new('RGBA', (size, size), BG_COLOR)
    draw = ImageDraw.Draw(img)

    # Scale font size with icon size
    font_size = max(10, int(size * 0.56))
    font = get_font(font_size)

    # Center text
    bbox = draw.textbbox((0, 0), TEXT, font=font)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    x = (size - tw) // 2
    y = (size - th) // 2 - int(size * 0.06)  # Slight vertical adjustment
    draw.text((x, y), TEXT, fill=TEXT_COLOR, font=font)

    return img


def main():
    print("Creating linefeed icons...")

    # Create 32x32 PNG for embedding (icon_data.rs)
    icon_32 = create_icon(32)
    icon_32.save(ICON_PNG)
    print(f"  Created {ICON_PNG.name} (32x32)")

    # Create multi-size ICO for Windows
    sizes = [16, 24, 32, 48, 64, 128, 256]
    icons = [create_icon(s) for s in sizes]
    icons[-1].save(ICON_ICO, format='ICO', append_images=icons[:-1])
    print(f"  Created {ICON_ICO.name} with sizes: {sizes}")

    print("\nDone! Run tools/gen-icon.py to update icon_data.rs")


if __name__ == "__main__":
    main()
