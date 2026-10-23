"""Draws the pack's tarp: a weathered canvas sheet with a stitched hem,
brass grommets tied with twine at the corners, and a few stains and
creases. RGBA, transparent around the slightly uneven edge.

Usage: python make_tarp.py OUT_DIR"""
import math, random, sys
from PIL import Image, ImageDraw, ImageFilter, ImageChops

W, H = 1200, 820
MARGIN = 26


def noise(w, h, scale, seed):
    """Smooth value noise in 0..255 (blurred random grid, upsampled)."""
    rnd = random.Random(seed)
    gw, gh = max(2, w // scale), max(2, h // scale)
    small = Image.new("L", (gw, gh))
    small.putdata([rnd.randint(0, 255) for _ in range(gw * gh)])
    return small.resize((w, h), Image.BICUBIC).filter(ImageFilter.GaussianBlur(scale / 3))


def edge_mask():
    """The sheet's outline: a rectangle whose edges wander a few pixels."""
    rnd = random.Random(7)
    pts = []
    def wobble(n):
        off, out = 0.0, []
        for _ in range(n):
            off = off * 0.85 + rnd.uniform(-1.6, 1.6)
            out.append(off)
        return out
    n = 60
    top, right, bottom, left = wobble(n), wobble(n), wobble(n), wobble(n)
    for i in range(n):
        pts.append((MARGIN + (W - 2 * MARGIN) * i / n, MARGIN + top[i]))
    for i in range(n):
        pts.append((W - MARGIN + right[i], MARGIN + (H - 2 * MARGIN) * i / n))
    for i in range(n):
        pts.append((W - MARGIN - (W - 2 * MARGIN) * i / n, H - MARGIN + bottom[i]))
    for i in range(n):
        pts.append((MARGIN + left[i], H - MARGIN - (H - 2 * MARGIN) * i / n))
    mask = Image.new("L", (W, H), 0)
    ImageDraw.Draw(mask).polygon(pts, fill=255)
    return mask.filter(ImageFilter.GaussianBlur(0.8))


def canvas():
    base = (196, 176, 132)
    img = Image.new("RGB", (W, H), base)
    px = img.load()
    # Weave: faint alternating warp and weft threads.
    for y in range(H):
        for x in range(W):
            warp = 6 if (x // 2) % 2 == 0 else -4
            weft = 5 if (y // 2) % 2 == 0 else -5
            d = (warp + weft) // 2
            r, g, b = base
            px[x, y] = (r + d, g + d, b + d)
    # Uneven dye and dirt.
    blotch = noise(W, H, 90, 3)
    fine = noise(W, H, 14, 4)
    dark = Image.new("RGB", (W, H), (120, 98, 66))
    img = Image.composite(dark, img, blotch.point(lambda v: max(0, v - 150) * 2))
    img = Image.blend(img, Image.merge("RGB", (fine, fine, fine)), 0.06)
    return img


def creases(img):
    """Soft folds: a blurred dark band with a lighter one beside it."""
    layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    rnd = random.Random(11)
    for _ in range(6):
        x0, y0 = rnd.uniform(120, W - 120), rnd.uniform(120, H - 120)
        a = rnd.uniform(0, math.pi)
        length = rnd.uniform(200, 420)
        pts = []
        for i in range(9):
            t = i / 8 - 0.5
            bend = 18 * math.sin(i * 0.8 + x0)
            pts.append((x0 + math.cos(a) * length * t - math.sin(a) * bend,
                        y0 + math.sin(a) * length * t + math.cos(a) * bend))
        d.line(pts, fill=(70, 52, 32, 70), width=14, joint="curve")
        d.line([(x + 9, y + 9) for x, y in pts], fill=(255, 240, 210, 55), width=10, joint="curve")
    layer = layer.filter(ImageFilter.GaussianBlur(7))
    return Image.alpha_composite(img.convert("RGBA"), layer).convert("RGB")


def stains(img):
    """A few faint, soft-edged water marks."""
    layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    rnd = random.Random(5)
    for _ in range(4):
        cx, cy, r = rnd.uniform(160, W - 160), rnd.uniform(160, H - 160), rnd.uniform(40, 90)
        d.ellipse([cx - r, cy - r * 0.75, cx + r, cy + r * 0.75], fill=(105, 80, 50, 26))
    layer = layer.filter(ImageFilter.GaussianBlur(16))
    return Image.alpha_composite(img.convert("RGBA"), layer).convert("RGB")


def hem(img):
    d = ImageDraw.Draw(img, "RGBA")
    inset = MARGIN + 22
    # A folded band along each edge, then a running stitch.
    d.rectangle([MARGIN, MARGIN, W - MARGIN, H - MARGIN], outline=(120, 98, 66, 90), width=20)
    step, dash = 22, 12
    for x in range(inset, W - inset, step):
        d.line([(x, inset), (x + dash, inset)], fill=(70, 52, 32, 200), width=3)
        d.line([(x, H - inset), (x + dash, H - inset)], fill=(70, 52, 32, 200), width=3)
    for y in range(inset, H - inset, step):
        d.line([(inset, y), (inset, y + dash)], fill=(70, 52, 32, 200), width=3)
        d.line([(W - inset, y), (W - inset, y + dash)], fill=(70, 52, 32, 200), width=3)
    return img


def grommets(img):
    d = ImageDraw.Draw(img, "RGBA")
    c = MARGIN + 38
    for (x, y) in [(c, c), (W - c, c), (c, H - c), (W - c, H - c)]:
        d.ellipse([x - 17, y - 17, x + 17, y + 17], fill=(150, 118, 60, 255), outline=(90, 66, 30, 255), width=3)
        d.ellipse([x - 8, y - 8, x + 8, y + 8], fill=(40, 30, 20, 255))
        # Twine looped through and trailing off the corner.
        sx = -1 if x < W / 2 else 1
        sy = -1 if y < H / 2 else 1
        pts = [(x, y), (x + sx * 10, y + sy * 6), (x + sx * 16, y + sy * 16), (x + sx * 12, y + sy * 28)]
        d.line(pts, fill=(150, 120, 80, 255), width=6, joint="curve")
        d.line(pts, fill=(185, 155, 105, 255), width=2, joint="curve")
    return img


def main(out_dir):
    img = canvas()
    img = creases(img)
    img = stains(img)
    img = hem(img)
    img = grommets(img)
    # Slight vignette toward the edges so items read on the middle.
    shade = Image.new("L", (W, H), 0)
    ImageDraw.Draw(shade).rectangle([0, 0, W, H], fill=0)
    inner = Image.new("L", (W, H), 0)
    ImageDraw.Draw(inner).rectangle([140, 120, W - 140, H - 120], fill=255)
    inner = inner.filter(ImageFilter.GaussianBlur(120))
    dark = Image.new("RGB", (W, H), (110, 88, 58))
    img = Image.composite(img, dark, inner.point(lambda v: 150 + v * 105 // 255))
    rgba = img.convert("RGBA")
    rgba.putalpha(edge_mask())
    # A soft shadow under the sheet, inside the same canvas.
    shadow = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    sm = edge_mask().filter(ImageFilter.GaussianBlur(10)).point(lambda v: v * 110 // 255)
    shadow.putalpha(ImageChops.offset(sm, 6, 9))
    out = Image.alpha_composite(shadow, rgba)
    out.save(f"{out_dir}/tarp.png")


if __name__ == "__main__":
    main(sys.argv[1])
