"""Draws the body-state HUD icons: white charcoal-like strokes on a soft dark
disc, 64x64 RGBA. Tinted per category at runtime."""
import math, random, sys
from PIL import Image, ImageDraw, ImageFilter

S = 256  # supersample
OUT = 64

def stroke(d, pts, w=14, seed=0):
    """A wobbly, slightly uneven line through pts, like charcoal."""
    rnd = random.Random(seed)
    dense = []
    for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
        n = max(2, int(math.hypot(x1 - x0, y1 - y0) / 6))
        for i in range(n):
            t = i / n
            dense.append((x0 + (x1 - x0) * t, y0 + (y1 - y0) * t))
    dense.append(pts[-1])
    off = 0.0
    jit = []
    for x, y in dense:
        off = off * 0.9 + rnd.uniform(-0.7, 0.7)
        jit.append((x + off, y + off * 0.6))
    for i, (a, b) in enumerate(zip(jit, jit[1:])):
        ww = w * (0.94 + 0.08 * math.sin(i * 0.31 + seed))
        d.line([a, b], fill=(255, 255, 255, 255), width=int(ww))
        r = ww / 2
        d.ellipse([b[0] - r, b[1] - r, b[0] + r, b[1] + r], fill=(255, 255, 255, 255))

def arc(cx, cy, r, a0, a1, n=40):
    return [(cx + r * math.cos(math.radians(a0 + (a1 - a0) * i / n)),
             cy + r * math.sin(math.radians(a0 + (a1 - a0) * i / n))) for i in range(n + 1)]

def drop(cx, cy, s):
    # teardrop: point at top, round bottom
    pts = [(cx, cy - 1.0 * s)]
    pts += arc(cx, cy + 0.25 * s, 0.55 * s, -30, 210)
    pts += [(cx, cy - 1.0 * s)]
    return pts

def cold(d):
    c = S / 2
    for k in range(3):
        a = math.radians(90 + 60 * k)
        dx, dy = math.cos(a) * 78, math.sin(a) * 78
        stroke(d, [(c - dx, c - dy), (c + dx, c + dy)], seed=k)
        for sgn in (1, -1):
            bx, by = c + sgn * dx * 0.6, c + sgn * dy * 0.6
            for side in (1, -1):
                b = a + side * math.radians(35) + (0 if sgn > 0 else math.pi)
                stroke(d, [(bx, by), (bx + math.cos(b) * 26, by + math.sin(b) * 26)], w=10, seed=k * 7 + side + sgn)

def wet(d):
    stroke(d, drop(S / 2 - 30, S / 2 - 8, 62), seed=1)
    stroke(d, drop(S / 2 + 44, S / 2 + 30, 36), w=11, seed=2)

def injury(d):
    c = S / 2
    pts = arc(c - 34, c - 22, 36, 150, 350) + arc(c + 34, c - 22, 36, 190, 390)
    pts = [(c, c + 78)] + [(c - 68, c - 4)] + pts[1:-1] + [(c + 68, c - 4), (c, c + 78)]
    stroke(d, pts, seed=3)
    # a slash: torn
    stroke(d, [(c - 14, c - 36), (c + 6, c - 6), (c - 8, c + 10), (c + 10, c + 38)], w=9, seed=4)

def thirst(d):
    c = S / 2
    stroke(d, drop(c, c - 4, 78), seed=5)
    # crack through the empty drop
    stroke(d, [(c - 6, c - 54), (c + 10, c - 20), (c - 10, c + 4), (c + 8, c + 40)], w=9, seed=6)

def exhaustion(d):
    c = S / 2
    # crescent moon
    outer = arc(c - 8, c, 74, 60, 300)
    inner = arc(c + 22, c - 6, 56, 250, 110 - 360 + 360)
    stroke(d, outer + list(reversed(arc(c + 24, c - 4, 58, 70, 290))), seed=7)
    # small z
    stroke(d, [(c + 46, c - 74), (c + 76, c - 74), (c + 46, c - 44), (c + 76, c - 44)], w=9, seed=8)

def hunger(d):
    c = S / 2
    # drumstick at 45 degrees: an egg of meat upper-left, bone lower-right
    ang = math.radians(45)
    def rot(x, y):
        return (c + x * math.cos(ang) - y * math.sin(ang), c + x * math.sin(ang) + y * math.cos(ang))
    meat = []
    for i in range(41):
        t = math.radians(35 + 290 * i / 40)
        # egg: fatter at the far end
        rx = 60 if math.cos(t) < 0 else 44
        meat.append(rot(-22 + rx * math.cos(t), 40 * math.sin(t)))
    stroke(d, meat, seed=9)
    a, b = rot(22, -12), rot(22, 12)
    stroke(d, [a, rot(70, -7)], w=12, seed=10)
    stroke(d, [b, rot(70, 7)], w=12, seed=12)
    for y in (-14, 14):
        x, yy = rot(80, y)
        stroke(d, arc(x, yy, 13, 0, 360, 18), w=11, seed=11 + y)
    # one bite taken
    stroke(d, arc(*rot(-40, -36), 16, 200, 340, 10), w=10, seed=13)

ICONS = dict(cold=cold, wet=wet, injury=injury, thirst=thirst, exhaustion=exhaustion, hunger=hunger)

def render(fn):
    disc = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(disc).ellipse([10, 10, S - 10, S - 10], fill=(12, 10, 8, 150))
    disc = disc.filter(ImageFilter.GaussianBlur(8))
    ink = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    fn(ImageDraw.Draw(ink))
    # rough the edges a touch, like charcoal on paper
    ink = ink.filter(ImageFilter.GaussianBlur(1.2))
    out = Image.alpha_composite(disc, ink)
    return out.resize((OUT, OUT), Image.LANCZOS)

if __name__ == "__main__":
    out_dir = sys.argv[1]
    sheet = Image.new("RGBA", (OUT * 6 * 2, OUT * 2), (90, 110, 80, 255))
    for i, (name, fn) in enumerate(ICONS.items()):
        img = render(fn)
        img.save(f"{out_dir}/{name}.png")
        sheet.alpha_composite(img.resize((OUT * 2, OUT * 2), Image.LANCZOS), (i * OUT * 2, 0))
    sheet.save(sys.argv[2])
