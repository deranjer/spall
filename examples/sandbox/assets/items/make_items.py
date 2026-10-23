"""Draws the sandbox's item icons for the pack: painted, slightly wobbly
shapes with a dark outline and simple shading, on transparency, 160x160.

Usage: python make_items.py OUT_DIR [CONTACT_SHEET.png]"""
import math, random, sys
from PIL import Image, ImageDraw, ImageFilter

S = 640  # supersample
OUT = 160
INK = (52, 38, 26, 255)


def wobble(pts, amount, seed):
    rnd = random.Random(seed)
    return [(x + rnd.uniform(-amount, amount), y + rnd.uniform(-amount, amount)) for x, y in pts]


def ellipse_pts(cx, cy, rx, ry, n=48, a0=0.0, a1=2 * math.pi):
    return [(cx + rx * math.cos(a0 + (a1 - a0) * i / n), cy + ry * math.sin(a0 + (a1 - a0) * i / n)) for i in range(n + 1)]


def shape(d, pts, fill, seed, outline=INK, width=14):
    pts = wobble(pts, 3.0, seed)
    d.polygon(pts, fill=fill)
    d.line(pts + [pts[0]], fill=outline, width=width, joint="curve")


def log(d):
    # A short log lying diagonally: bark body, pale cut end with rings.
    a = math.radians(-24)
    ca, sa = math.cos(a), math.sin(a)
    def rot(x, y):
        return (S / 2 + x * ca - y * sa, S / 2 + 20 + x * sa + y * ca)
    half_l, r = 210, 92
    body = [rot(-half_l, -r), rot(half_l, -r)] + [rot(half_l + 0.35 * r * math.cos(t), r * math.sin(t)) for t in [-math.pi / 2 + math.pi * i / 16 for i in range(17)]] + [rot(-half_l, r)]
    shape(d, body, (112, 76, 44, 255), 1)
    # Bark grooves.
    rnd = random.Random(2)
    for i in range(7):
        y = -r + 22 + i * (2 * r - 44) / 6
        x0 = -half_l + 30 + rnd.uniform(0, 40)
        x1 = half_l - 60 - rnd.uniform(0, 60)
        d.line([rot(x0, y), rot((x0 + x1) / 2, y + rnd.uniform(-8, 8)), rot(x1, y)], fill=(78, 52, 30, 255), width=8)
    # Cut end.
    end = [rot(-half_l + 0.35 * r * math.cos(t), r * math.sin(t)) for t in [2 * math.pi * i / 40 for i in range(41)]]
    shape(d, end, (214, 178, 122, 255), 3)
    for k in (0.66, 0.36):
        ring = [rot(-half_l + 0.35 * r * k * math.cos(t), r * k * math.sin(t)) for t in [2 * math.pi * i / 32 for i in range(33)]]
        d.line(ring, fill=(160, 120, 72, 255), width=6)
    # Highlight along the top.
    d.line([rot(-half_l + 40, -r + 14), rot(half_l - 40, -r + 14)], fill=(150, 108, 66, 255), width=10)


def plank(d):
    a = math.radians(-18)
    ca, sa = math.cos(a), math.sin(a)
    def rot(x, y):
        return (S / 2 + x * ca - y * sa, S / 2 + x * sa + y * ca)
    hl, hw, t = 240, 62, 26
    top = [rot(-hl, -hw), rot(hl, -hw), rot(hl, hw), rot(-hl, hw)]
    side = [rot(-hl, hw), rot(hl, hw), rot(hl, hw + t), rot(-hl, hw + t)]
    shape(d, side, (150, 106, 58, 255), 4)
    shape(d, top, (206, 160, 98, 255), 5)
    # Grain.
    rnd = random.Random(6)
    for i in range(5):
        y = -hw + 20 + i * (2 * hw - 40) / 4
        pts = [rot(-hl + 24 + j * (2 * hl - 48) / 8, y + 6 * math.sin(j * 0.9 + i)) for j in range(9)]
        d.line(pts, fill=(168, 122, 70, 255), width=5)
    # A knot.
    kx, ky = rot(80, -10)
    d.ellipse([kx - 18, ky - 11, kx + 18, ky + 11], outline=(120, 82, 44, 255), width=6)


def stone(d):
    # A rough lump with a lit face and a few facets.
    c = S / 2
    rnd = random.Random(8)
    pts = []
    for i in range(11):
        t = 2 * math.pi * i / 11
        r = 190 + rnd.uniform(-35, 25)
        pts.append((c + r * math.cos(t), c + 30 + r * 0.78 * math.sin(t)))
    shape(d, pts, (128, 128, 132, 255), 9)
    face = [(c - 120, c - 40), (c + 20, c - 120), (c + 130, c - 60), (c + 40, c + 30), (c - 90, c + 20)]
    shape(d, face, (166, 166, 170, 255), 10, outline=(90, 90, 96, 255), width=6)
    d.line([(c + 40, c + 30), (c + 110, c + 140)], fill=(80, 80, 86, 255), width=6)
    d.line([(c - 90, c + 20), (c - 150, c + 110)], fill=(80, 80, 86, 255), width=6)
    # Speckles.
    for _ in range(30):
        x, y = c + rnd.uniform(-150, 150), c + 30 + rnd.uniform(-110, 120)
        d.ellipse([x - 4, y - 4, x + 4, y + 4], fill=(100, 100, 104, 255))


def stone_block(d):
    # A squared block in simple three-quarter view.
    c = S / 2
    s, k = 150, 70
    top = [(c - s, c - 60), (c, c - 60 - k), (c + s, c - 60), (c, c - 60 + k)]
    left = [(c - s, c - 60), (c, c - 60 + k), (c, c + 160 + k - 60), (c - s, c + 160 - 60)]
    right = [(c, c - 60 + k), (c + s, c - 60), (c + s, c + 160 - 60), (c, c + 160 + k - 60)]
    shape(d, left, (118, 118, 124, 255), 11)
    shape(d, right, (96, 96, 102, 255), 12)
    shape(d, top, (168, 168, 174, 255), 13)
    # Chisel marks.
    for i in range(4):
        y = c - 10 + i * 40
        d.line([(c - s + 30, y), (c - 30, y + 30)], fill=(98, 98, 104, 255), width=5)
        d.line([(c + 30, y + 30), (c + s - 30, y)], fill=(78, 78, 84, 255), width=5)


def stick(d):
    # A thin, slightly crooked branch with a side twig and a pale cut end.
    a = math.radians(-35)
    ca, sa = math.cos(a), math.sin(a)
    def rot(x, y):
        return (S / 2 + x * ca - y * sa, S / 2 + 10 + x * sa + y * ca)
    spine = [(-240, 6), (-120, -4), (0, 6), (120, -2), (240, 4)]
    top = [rot(x, y - 22) for x, y in spine]
    bottom = [rot(x, y + 22) for x, y in reversed(spine)]
    shape(d, top + bottom, (128, 90, 54, 255), 21, width=12)
    twig = [rot(40, -16), rot(110, -96), rot(124, -88), rot(66, -10)]
    shape(d, twig, (128, 90, 54, 255), 22, width=10)
    end = ellipse_pts(*rot(-240, 6), 12, 22, 24)
    shape(d, end, (214, 178, 122, 255), 23, width=8)
    d.line([rot(-200, -6), rot(200, -8)], fill=(156, 114, 70, 255), width=6)


def fire_pit(d):
    # A ring of stones round a little teepee of sticks over glowing embers.
    c = S / 2
    d.ellipse([c - 120, c + 60, c + 120, c + 150], fill=(230, 120, 40, 255))
    d.ellipse([c - 70, c + 80, c + 70, c + 130], fill=(255, 200, 90, 255))
    for i, (x0, x1) in enumerate([(-110, 20), (110, -20), (-40, 30), (40, -30)]):
        shape(d, [(c + x0 - 10, c + 120), (c + x1 - 8, c - 150), (c + x1 + 8, c - 150), (c + x0 + 10, c + 120)],
              (128, 90, 54, 255), 30 + i, width=10)
    rnd = random.Random(31)
    for i in range(8):
        t = math.pi * (0.05 + 0.9 * i / 7)
        x, y = c + 230 * math.cos(t), c + 120 + 60 * math.sin(t)
        r = 52 + rnd.uniform(-8, 8)
        pts = [(x + r * math.cos(k * 2 * math.pi / 7), y + r * 0.72 * math.sin(k * 2 * math.pi / 7)) for k in range(7)]
        shape(d, pts, (132, 132, 136, 255), 40 + i, width=10)
        d.ellipse([x - r * 0.4, y - r * 0.5, x + r * 0.2, y - r * 0.1], fill=(168, 168, 172, 255))


ICONS = dict(log=log, plank=plank, stone=stone, stone_block=stone_block, stick=stick, fire_pit=fire_pit)


def render(fn):
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    fn(ImageDraw.Draw(img))
    # A soft contact shadow under the item.
    shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).ellipse([S * 0.18, S * 0.72, S * 0.82, S * 0.9], fill=(30, 20, 10, 110))
    shadow = shadow.filter(ImageFilter.GaussianBlur(22))
    out = Image.alpha_composite(shadow, img)
    return out.resize((OUT, OUT), Image.LANCZOS)


if __name__ == "__main__":
    out_dir = sys.argv[1]
    sheet = Image.new("RGBA", (OUT * len(ICONS), OUT), (196, 176, 132, 255))
    for i, (name, fn) in enumerate(ICONS.items()):
        img = render(fn)
        img.save(f"{out_dir}/{name}.png")
        sheet.alpha_composite(img, (i * OUT, 0))
    if len(sys.argv) > 2:
        sheet.save(sys.argv[2])
