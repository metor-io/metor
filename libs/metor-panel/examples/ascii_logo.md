# Terminal rosette

An ASCII or Unicode rendering of [`../logo.svg`](../logo.svg): 17 overlapping ellipses,
rotated in 10-degree steps, with 3-unit strokes. Each stroke's orange gradient
fades from 100% to 20% opacity; overlaps use source-over alpha compositing.
Inspired by the dotted Amp welcome screen and the mathematical animation
approach described in [aiscii](https://ossama.is/writing/aiscii).

Five animation ideas are implemented:

- **Weave** (default): a slow rotation of the original logo and its gradients.
- **Breath**: gentle expansion/contraction alongside the rotation.
- **Orbit**: a stronger rotation with a rocking tilt of the logo's plane.
- **Ellipses**: alternating ellipses rotate in opposite directions around the
  shared center, changing the overlap pattern. Each gradient follows its ellipse.
- **Opacity**: the geometry stays still while the gradients fade to fully opaque
  strokes and back over six seconds. The dim opening on the right closes and
  reopens; the central hole stays open.

Run from the `metor-panel` directory with Python 3.9 or newer. No packages are
required; keep `logo.svg` in the directory above the script. Interactive mode
supports macOS, Linux, and WSL.

```sh
python3 examples/ascii_logo.py
python3 examples/ascii_logo.py --mode orbit --palette amp
python3 examples/ascii_logo.py --mode breath --speed 0.6
python3 examples/ascii_logo.py --mode ellipses
python3 examples/ascii_logo.py --mode opacity
python3 examples/ascii_logo.py --mode ellipses --unicode
```

Press **1 / 2 / 3 / 4 / 5** to switch motion, **c** to cycle orange, green/blue, and
monochrome, **u** to toggle ASCII/Unicode, **space** to pause, and **q** or **Ctrl-C**
to exit. Unicode uses dots and filled circles (`·∙•●`) for a softer, dotted look
and works with all five animations and every palette. The active character set
appears in the title. The animation
adapts to terminal resizing and restores the previous screen on exit.
The default canvas is capped at 88 columns by 38 rows; expand it with
`--width 120 --height 48`. True color looks best on a dark terminal theme.

For a static frame, piping, or a timed demo:

```sh
python3 examples/ascii_logo.py --frame 2 --color
python3 examples/ascii_logo.py --frame 2 > /tmp/rosette.txt
python3 examples/ascii_logo.py --duration 10 --fps 24
```

Piped output defaults to one plain ASCII frame. `--no-color` and `NO_COLOR`
disable ANSI colors. `--color` overrides `NO_COLOR` and enables captured colors.
`render(width, height, seconds, mode)` is separate from terminal I/O if you
want to reuse the animation in another interface. The loader rasterizes the
SVG's actual cubic paths once as separate layers, then samples their opacity
for each terminal cell. At `--frame 0`, geometry and gradients have their original
orientation. Terminal character resolution limits the fine detail; a larger
canvas shows more of the weave. The small SVG reader supports this file's
export format, not arbitrary SVG features.

The panel's startup screen embeds a Rust renderer in
[`src/connections/logo.rs`](../src/connections/logo.rs). It embeds the same SVG
and renders a stationary 48-by-24 Unicode character grid. Each gradient's fading
stop breathes between 0 and 0.3 opacity over twenty seconds using smootherstep
easing; the first stop remains fully opaque. Stroke
coverage is cached, then the gradients and overlaps are recomposited each frame.
Reduced motion holds the fading stop at 0.3 opacity. No Python installation or
runtime SVG file is required.
