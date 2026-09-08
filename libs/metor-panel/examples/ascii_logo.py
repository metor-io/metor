#!/usr/bin/env python3
"""Animate ../logo.svg with ASCII or Unicode. Python 3.9+, standard library only.

Run: python3 examples/ascii_logo.py
Keys: 1-5 change motion, u toggles Unicode, c changes palette, space pauses, q quits.
"""

import argparse
import math
import os
import re
import select
import shutil
import signal
import sys
import time
import xml.etree.ElementTree as ET
from contextlib import contextmanager
from functools import lru_cache
from pathlib import Path


MODES = ("weave", "breath", "orbit", "ellipses", "opacity")
PALETTES = ("ember", "amp", "mono")
RAMPS = {"ascii": " .:oO@", "unicode": " ·∙•●"}
BACKGROUND = "\033[48;2;24;28;28m"
RESET = "\033[0m"
SVG_PATH = Path(__file__).resolve().parents[1] / "logo.svg"


def path_points(data):
    """Flatten the absolute M/C/Z cubic paths used by this logo's SVG export."""
    match = re.fullmatch(r"M([^A-Za-z]+)((?:C[^A-Za-z]+)+)Z", data.strip())
    if not match:
        raise ValueError("logo paths must use one M, repeated cubic C coordinates, and Z")
    numbers = lambda text: [float(n) for n in re.findall(r"[-+]?(?:\d*\.\d+|\d+)", text)]
    start = numbers(match[1])
    values = numbers(match[2])
    if len(start) != 2 or len(values) % 6:
        raise ValueError("invalid cubic path coordinates")
    points = [tuple(start)]
    for i in range(0, len(values), 6):
        p0 = points[-1]
        p1, p2, p3 = [values[j:j + 2] for j in range(i, i + 6, 2)]
        for step in range(1, 13):
            t = step / 12
            s = 1 - t
            points.append(tuple(s**3 * p0[k] + 3*s*s*t*p1[k] +
                                3*s*t*t*p2[k] + t**3*p3[k] for k in (0, 1)))
    points.append(points[0])
    return points


@lru_cache(maxsize=1)
def logo_layers():
    """Keep each SVG stroke separate so its angle and opacity can animate.

    This deliberately handles the supplied SVG's cubic paths and two-stop
    opacity gradients, rather than implementing a general-purpose SVG engine.
    """
    root = ET.parse(SVG_PATH).getroot()
    ns = {"s": "http://www.w3.org/2000/svg"}
    gradients = {g.get("id"): g for g in root.findall(".//s:linearGradient", ns)}
    vx, vy, vw, vh = map(float, root.get("viewBox").split())
    size = 306
    sx, sy = size / vw, size / vh
    layers = []
    for path in root.findall("s:path", ns):
        points = [((x-vx)*sx, (y-vy)*sy) for x, y in path_points(path.get("d"))]
        radius = float(path.get("stroke-width")) * sx / 2
        coverage = [0.0] * (size * size)
        for (ax, ay), (bx, by) in zip(points, points[1:]):
            dx, dy = bx-ax, by-ay
            length2 = dx*dx + dy*dy
            if length2 < 1e-12:
                continue
            for y in range(max(0, int(min(ay, by)-radius-1)), min(size, math.ceil(max(ay, by)+radius+1))):
                for x in range(max(0, int(min(ax, bx)-radius-1)), min(size, math.ceil(max(ax, bx)+radius+1))):
                    t = max(0, min(1, ((x+0.5-ax)*dx + (y+0.5-ay)*dy) / length2))
                    distance = math.hypot(x+0.5-ax-t*dx, y+0.5-ay-t*dy)
                    index = y*size + x
                    coverage[index] = max(coverage[index], min(1, radius+0.5-distance))
        gradient = gradients[path.get("stroke")[5:-1]]
        x1, y1, x2, y2 = (float(gradient.get(k)) for k in ("x1", "y1", "x2", "y2"))
        dx, dy = x2-x1, y2-y1
        stops = gradient.findall("s:stop", ns)
        a, b = (float(stop.get("stop-opacity", "1")) for stop in stops)
        end = float(stops[1].get("offset"))
        samples = []
        texture = [0.0] * (size * size)
        for index, ink in enumerate(coverage):
            if ink <= 0:
                continue
            x, y = (index % size + 0.5)/sx + vx, (index // size + 0.5)/sy + vy
            t = max(0, min(1, ((x-x1)*dx + (y-y1)*dy) / ((dx*dx+dy*dy)*end)))
            alpha = ink * (a + (b-a)*t)
            texture[index] = alpha
            samples.append((index, ink, alpha))
        points = [(x*2/size-1, y*2/size-1) for x, y in points]
        layers.append((points, radius*2/size, samples, texture))
    return size, layers


def composite_opacity(amount):
    size, layers = logo_layers()
    texture = [0.0] * (size * size)
    for _, _, samples, _ in layers:
        for index, ink, alpha in samples:
            alpha += (ink-alpha) * amount
            texture[index] += (1-texture[index]) * alpha
    return size, texture


@lru_cache(maxsize=1)
def logo_texture():
    return composite_opacity(0)


def rotating_ellipses(width, height, seconds):
    size, layers = logo_layers()
    scale = min((width-4)/4.4, (height-2)/2.2) * 2
    cells = [[0.0] * width for _ in range(height*2)]
    if scale <= 0:
        return [[0.0] * width for _ in range(height)]
    cx, cy = (width-1)/2, (height*2-1)/2
    for strand, (points, radius, _, texture) in enumerate(layers):
        # Alternating directions change the weave while keeping a shared center.
        angle = seconds * (0.28 if strand % 2 else -0.22)
        cs, sn = math.cos(angle), math.sin(angle)
        rotated = [(x*cs-y*sn, x*sn+y*cs) for x, y in points]
        xs, ys = zip(*rotated)
        left = max(0, math.floor(cx + (min(xs)-radius)*scale)-1)
        right = min(width, math.ceil(cx + (max(xs)+radius)*scale)+2)
        top = max(0, math.floor(cy + (min(ys)-radius)*scale)-1)
        bottom = min(height*2, math.ceil(cy + (max(ys)+radius)*scale)+2)
        for row in range(top, bottom):
            y = (row-cy)/scale
            for col in range(left, right):
                x = (col-cx)/scale
                u = (x*cs+y*sn+1)*size/2-0.5
                v = (-x*sn+y*cs+1)*size/2-0.5
                ix, iy = math.floor(u), math.floor(v)
                if 0 <= ix < size-1 and 0 <= iy < size-1:
                    fx, fy = u-ix, v-iy
                    i = iy*size+ix
                    alpha = ((1-fx)*texture[i]+fx*texture[i+1])*(1-fy)
                    alpha += ((1-fx)*texture[i+size]+fx*texture[i+size+1])*fy
                    cells[row][col] += (1-cells[row][col])*alpha
    return [[(a+b)/2 for a, b in zip(cells[y], cells[y+1])]
            for y in range(0, height*2, 2)]


def render(width, height, seconds, mode="weave"):
    """Return character intensities; independent of terminal I/O for embedding."""
    if mode == "ellipses":
        return rotating_ellipses(width, height, seconds)
    cells = [[0.0] * width for _ in range(height)]
    size, texture = logo_texture()
    if mode == "opacity":
        # Six seconds from the original gradient to opaque strokes and back.
        size, texture = composite_opacity((1-math.cos(seconds*math.tau/6))/2)
    # A character is roughly twice as tall as it is wide.
    scale = min((width - 4) / 4.4, (height - 2) / 2.2)
    if scale <= 0:
        return cells
    spin = 0 if mode == "opacity" else seconds * 0.10
    tilt = 0.0
    if mode == "orbit":
        tilt = 0.85 * math.sin(seconds * 0.40)
        spin = seconds * 0.23
    cs, sn = math.cos(spin), math.sin(spin)
    scale *= 1 + (0.045 * math.sin(seconds * 1.5) if mode == "breath" else 0)
    ct = math.cos(tilt)
    # Supersample each cell after compositing, so tiny holes survive resizing.
    for row in range(height):
        for col in range(width):
            total = 0.0
            for oy in (-0.375, -0.125, 0.125, 0.375):
                y = (row + oy - (height-1)/2) / (scale*ct)
                for ox in (-0.25, 0.25):
                    x = (col + ox - (width-1)/2) / (scale*2)
                    u = ((x*cs + y*sn) + 1) * size/2 - 0.5
                    v = ((-x*sn + y*cs) + 1) * size/2 - 0.5
                    ix, iy = math.floor(u), math.floor(v)
                    if 0 <= ix < size-1 and 0 <= iy < size-1:
                        fx, fy = u-ix, v-iy
                        i = iy*size + ix
                        total += ((1-fx)*texture[i] + fx*texture[i+1])*(1-fy)
                        total += ((1-fx)*texture[i+size] + fx*texture[i+size+1])*fy
            cells[row][col] = total / 8
    return cells


def rgb(level, palette):
    stops = ((24, 28, 28), (140, 54, 14), (255, 79, 0))
    if palette == "amp":
        stops = ((34, 32, 100), (41, 151, 150), (100, 255, 161))
    value = min(1.0, level) * 2
    a, b = stops[min(1, int(value)) : min(1, int(value)) + 2]
    fraction = value - min(1, int(value))
    return tuple(round(x + (y - x) * fraction) for x, y in zip(a, b))


def paint(cells, palette, color, glyphs="ascii"):
    ramp = RAMPS[glyphs]
    lines = []
    for row in cells:
        parts, last = [], None
        for value in row:
            level = min(len(ramp) - 1, round(value * (len(ramp) - 1)))
            char = ramp[level]
            shade = round(value * 23)
            if color and level and shade != last:
                r, g, b = rgb(shade / 23, palette)
                parts.append(f"\033[38;2;{r};{g};{b}m")
                last = shade
            parts.append(char)
        lines.append("".join(parts))
    return lines


def frame(width, height, seconds, mode, palette, color, paused=False, glyphs="ascii"):
    art_height = max(1, height - 4)
    lines = paint(render(width, art_height, seconds, mode), palette, color, glyphs)
    label = f"m e t o r   /   {mode}   /   {glyphs}" + ("   [paused]" if paused else "")
    hint = "1 weave  2 breath  3 orbit  4 ellipses  5 opacity | u glyphs  c color  space pause  q quit"
    if width < len(hint):
        hint = "1-5 motion | u glyphs | c color | space pause | q quit"
    muted = "\033[38;2;133;137;128m" if color else ""
    lines = [muted + label[:width].center(width), " " * width] + lines
    lines += [" " * width, muted + hint[:width].center(width)]
    return (BACKGROUND if color else "") + "\n".join(lines[:height]) + (RESET if color else "")


@contextmanager
def terminal():
    # Save terminal state before entering cbreak; always restore it on exit.
    import termios
    import tty

    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    previous = {}

    def stop(signum, unused_frame):
        raise KeyboardInterrupt

    try:
        for sig in (signal.SIGTERM, signal.SIGHUP):
            previous[sig] = signal.signal(sig, stop)
        tty.setcbreak(fd)
        sys.stdout.write("\033[?1049h\033[?25l\033[2J")
        sys.stdout.flush()
        yield fd
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)
        sys.stdout.write(RESET + "\033[?25h\033[?1049l")
        sys.stdout.flush()
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def positive(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be a finite number greater than zero")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=MODES, default="weave")
    parser.add_argument("--palette", choices=PALETTES, default="ember")
    parser.add_argument("--unicode", dest="glyphs", action="store_const", const="unicode",
                        default="ascii", help="use Unicode dots and filled circles")
    parser.add_argument("--fps", type=positive, default=30, help="frames per second (default: 30)")
    parser.add_argument("--speed", type=positive, default=1, help="motion multiplier")
    parser.add_argument("--duration", type=positive, help="stop after this many seconds")
    parser.add_argument("--frame", type=float, metavar="SECONDS", help="print one frame at this time")
    parser.add_argument("--width", type=int, default=88, help="maximum canvas columns")
    parser.add_argument("--height", type=int, default=38, help="maximum canvas rows")
    colors = parser.add_mutually_exclusive_group()
    colors.add_argument("--color", action="store_true", help="force ANSI true color, including in pipes")
    colors.add_argument("--no-color", action="store_true")
    args = parser.parse_args()
    if args.width < 8 or args.height < 8:
        parser.error("width and height must be at least 8")
    if args.frame is not None and not math.isfinite(args.frame):
        parser.error("frame time must be finite")
    try:
        logo_texture()
    except (OSError, ET.ParseError, ValueError) as error:
        parser.error(f"could not load {SVG_PATH}: {error}")
    color = not args.no_color and (
        args.color or (sys.stdout.isatty() and "NO_COLOR" not in os.environ)
    )
    if args.frame is not None or not (sys.stdout.isatty() and sys.stdin.isatty()):
        print(frame(args.width, args.height, (args.frame or 0) * args.speed,
                    args.mode, args.palette, color and args.palette != "mono", glyphs=args.glyphs))
        return
    if os.name != "posix":
        parser.error("interactive mode needs macOS, Linux, or WSL; --frame works anywhere")

    mode, palette, paused, elapsed = args.mode, args.palette, False, 0.0
    glyphs = args.glyphs
    start = last = time.monotonic()
    with terminal() as fd:
        while True:
            tick = time.monotonic()
            if args.duration and tick - start >= args.duration:
                break
            if not paused:
                elapsed += (tick - last) * args.speed
            last = tick
            columns, rows = shutil.get_terminal_size()
            width, height = min(args.width, columns - 1), min(args.height, rows - 1)
            if width < 8 or height < 8:
                content = "Enlarge terminal"[:max(0, width)]
            else:
                content = frame(width, height, elapsed, mode, palette,
                                color and palette != "mono", paused, glyphs)
            top = max(0, (rows - height) // 2)
            left = max(0, (columns - width) // 2)
            # Repaint each line in place; reserve the last column to avoid wrapping.
            output = ["\033[H", BACKGROUND if color and palette != "mono" else RESET]
            output.extend("\033[2K\n" for _ in range(top))
            for line in content.split("\n"):
                output.extend(("\033[2K", " " * left, line, "\n"))
            output.append("\033[J")
            sys.stdout.write("".join(output))
            sys.stdout.flush()
            timeout = max(0, 1 / args.fps - (time.monotonic() - tick))
            if select.select([fd], [], [], timeout)[0]:
                for key in os.read(fd, 32).decode(errors="ignore"):
                    if key in ("q", "Q", "\x04"):
                        return
                    if key in "12345":
                        mode = MODES[int(key) - 1]
                    elif key == " ":
                        paused = not paused
                    elif key in ("c", "C"):
                        palette = PALETTES[(PALETTES.index(palette) + 1) % len(PALETTES)]
                    elif key in ("u", "U"):
                        glyphs = "unicode" if glyphs == "ascii" else "ascii"


if __name__ == "__main__":
    try:
        main()
    except (KeyboardInterrupt, BrokenPipeError):
        pass
