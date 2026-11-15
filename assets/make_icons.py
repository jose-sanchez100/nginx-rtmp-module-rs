"""Draws the project's icons: a green hexagon with a white play mark, and a
live variant with a red dot. Needs Pillow.

    python assets/make_icons.py assets
"""
import math, sys
from PIL import Image, ImageDraw
out = sys.argv[1]
S = 1024  # supersampled canvas
GREEN, DARK, RED, WHITE = (0, 150, 70, 255), (0, 110, 50, 255), (230, 30, 45, 255), (255, 255, 255, 255)

def hexagon(r, cx=S/2, cy=S/2):
    return [(cx + r*math.cos(math.radians(a)), cy + r*math.sin(math.radians(a))) for a in range(0, 360, 60)]

def base(play_scale=1.0):
    im = Image.new('RGBA', (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    R, rad = 470, 70  # outer radius, corner rounding
    # rounded hexagon: fill the inset polygon and stroke it with round joins
    inner = hexagon(R - rad)
    d.polygon(inner, fill=DARK)
    d.line(inner + [inner[0]], fill=DARK, width=2*rad, joint='curve')
    for x, y in inner:
        d.ellipse((x-rad, y-rad, x+rad, y+rad), fill=DARK)
    inner2 = hexagon(R - rad - 40)
    d.polygon(inner2, fill=GREEN)
    d.line(inner2 + [inner2[0]], fill=GREEN, width=2*rad, joint='curve')
    for x, y in inner2:
        d.ellipse((x-rad, y-rad, x+rad, y+rad), fill=GREEN)
    # play triangle, optically centred
    h = 400 * play_scale
    cx, cy = S/2 + h*0.12, S/2
    tri = [(cx - h*0.45, cy - h/2), (cx - h*0.45, cy + h/2), (cx + h*0.55, cy)]
    r2 = 40
    d.polygon(tri, fill=WHITE)
    d.line(tri + [tri[0]], fill=WHITE, width=2*r2, joint='curve')
    for x, y in tri:
        d.ellipse((x-r2, y-r2, x+r2, y+r2), fill=WHITE)
    return im

def live():
    im = base()
    d = ImageDraw.Draw(im)
    c, r, ring = (S*0.72, S*0.72), S*0.25, S*0.05
    d.ellipse((c[0]-r-ring, c[1]-r-ring, c[0]+r+ring, c[1]+r+ring), fill=WHITE)
    d.ellipse((c[0]-r, c[1]-r, c[0]+r, c[1]+r), fill=RED)
    return im

sizes = [16, 20, 24, 32, 48, 64, 128, 256]
for name, im in [('nginx.ico', base()), ('nginx-live.ico', live())]:
    im.resize((256, 256), Image.LANCZOS).save(f'{out}/{name}', sizes=[(s, s) for s in sizes])
