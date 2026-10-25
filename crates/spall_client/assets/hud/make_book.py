"""Draws the blueprint book (an open, worn leather journal with two cream
pages) and the crafting cloth (a square of plain canvas with a stitched hem).
RGBA, transparent outside.

Usage: python make_book.py OUT_DIR"""
import math, random, sys
from PIL import Image, ImageDraw, ImageFilter

BOOK_W, BOOK_H = 1400, 900
CLOTH = 720


def noise(w, h, scale, seed):
    rnd = random.Random(seed)
    gw, gh = max(2, w // scale), max(2, h // scale)
    small = Image.new("L", (gw, gh))
    small.putdata([rnd.randint(0, 255) for _ in range(gw * gh)])
    return small.resize((w, h), Image.BICUBIC).filter(ImageFilter.GaussianBlur(scale / 3))


def mottle(img, colour, scale, seed, strength):
    """Darkens `img` toward `colour` in soft blotches."""
    blotch = noise(img.width, img.height, scale, seed)
    dark = Image.new("RGB", img.size, colour)
    return Image.composite(dark, img, blotch.point(lambda v: int(max(0, v - 140) * strength)))


def book():
    w, h = BOOK_W, BOOK_H
    out = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    # Shadow, then the leather cover a little larger than the pages.
    shadow = Image.new("L", (w, h), 0)
    ImageDraw.Draw(shadow).rounded_rectangle([24, 30, w - 16, h - 10], 36, fill=130)
    out.putalpha(shadow.filter(ImageFilter.GaussianBlur(14)))
    cover = Image.new("RGB", (w, h), (92, 58, 34))
    cover = mottle(cover, (54, 32, 18), 60, 1, 1.6)
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).rounded_rectangle([14, 14, w - 26, h - 30], 34, fill=255)
    out.paste(cover, (0, 0), mask)
    d = ImageDraw.Draw(out)
    d.rounded_rectangle([30, 30, w - 42, h - 46], 26, outline=(150, 110, 66, 255), width=3)
    # The two pages, each slightly curled toward the spine.
    page_colour = (236, 224, 196)
    spine = (w - 12) // 2
    for left in (True, False):
        x0, x1 = (52, spine - 6) if left else (spine + 6, w - 64)
        page = Image.new("RGB", (x1 - x0, h - 120), page_colour)
        page = mottle(page, (196, 176, 136), 90, 3 if left else 4, 0.9)
        fine = noise(page.width, page.height, 10, 5 if left else 6)
        page = Image.blend(page, Image.merge("RGB", (fine, fine, fine)), 0.05)
        pm = Image.new("L", page.size, 0)
        ImageDraw.Draw(pm).rounded_rectangle([0, 0, page.width - 1, page.height - 1], 10, fill=255)
        out.paste(page, (x0, 56), pm)
        # Darker toward the spine: the gutter.
        gutter = Image.new("RGBA", page.size, (0, 0, 0, 0))
        gd = ImageDraw.Draw(gutter)
        for i in range(70):
            a = int(70 * (1 - i / 70) ** 2)
            x = page.width - 1 - i if left else i
            gd.line([(x, 0), (x, page.height)], fill=(70, 50, 30, a))
        out.alpha_composite(gutter, (x0, 56))
    # A ribbon bookmark's tail hanging below the right page.
    d.polygon([(w - 220, h - 66), (w - 196, h - 66), (w - 196, h - 6), (w - 208, h - 18), (w - 220, h - 6)],
              fill=(130, 36, 30, 255))
    return out


def cloth():
    s = CLOTH
    out = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    base = Image.new("RGB", (s, s), (214, 200, 168))
    px = base.load()
    for y in range(s):
        for x in range(s):
            d = (5 if (x // 2) % 2 == 0 else -4) + (4 if (y // 2) % 2 == 0 else -4)
            r, g, b = px[x, y]
            px[x, y] = (r + d // 2, g + d // 2, b + d // 2)
    base = mottle(base, (160, 140, 104), 70, 9, 1.2)
    mask = Image.new("L", (s, s), 0)
    rnd = random.Random(2)
    pts = []
    m = 14
    for side in range(4):
        for i in range(40):
            t = i / 40
            j = rnd.uniform(-1.5, 1.5)
            if side == 0:
                pts.append((m + (s - 2 * m) * t, m + j))
            elif side == 1:
                pts.append((s - m + j, m + (s - 2 * m) * t))
            elif side == 2:
                pts.append((s - m - (s - 2 * m) * t, s - m + j))
            else:
                pts.append((m + j, s - m - (s - 2 * m) * t))
    ImageDraw.Draw(mask).polygon(pts, fill=255)
    out.paste(base, (0, 0), mask.filter(ImageFilter.GaussianBlur(0.8)))
    d = ImageDraw.Draw(out)
    inset = 34
    for x in range(inset, s - inset, 20):
        d.line([(x, inset), (x + 11, inset)], fill=(80, 60, 38, 200), width=3)
        d.line([(x, s - inset), (x + 11, s - inset)], fill=(80, 60, 38, 200), width=3)
    for y in range(inset, s - inset, 20):
        d.line([(inset, y), (inset, y + 11)], fill=(80, 60, 38, 200), width=3)
        d.line([(s - inset, y), (s - inset, y + 11)], fill=(80, 60, 38, 200), width=3)
    return out


if __name__ == "__main__":
    out_dir = sys.argv[1]
    book().save(f"{out_dir}/book.png")
    cloth().save(f"{out_dir}/cloth.png")
