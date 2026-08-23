//! `compile` never panics, for any input at all.
//!
//! This is a hard obligation rather than a nicety: the panel compiles on a
//! debounce while someone is still typing, so half-written source is the
//! normal case. Every input below must come back as a `Result` — with spans
//! that actually point inside the source — and none may unwind or abort.
//!
//! The fuzzing here is deterministic. A seeded generator beats a random one
//! for a test that has to mean the same thing on every run, and the corpus is
//! deliberately made of realistic sources cut and dented rather than of
//! uniform noise, because that is what a keystroke produces.

use crate::compile;

const CORPUS: &[&str] = &[
    "def f() -> f64:\n    return 1.0\n",
    "def scale(x: f64, k: f64) -> f64:\n    return x * k\n",
    "def norm(x: f64, y: f64) -> f64:\n    return sqrt(x ** 2 + y ** 2)\n",
    "def clamp(x: f64, lo: f64, hi: f64) -> f64:\n\
     \x20   if x < lo:\n\
     \x20       return lo\n\
     \x20   elif x > hi:\n\
     \x20       return hi\n\
     \x20   else:\n\
     \x20       return x\n",
    "def count(n: i64) -> i64:\n\
     \x20   total = 0\n\
     \x20   i = 0\n\
     \x20   while i < n:\n\
     \x20       if i % 3 == 0:\n\
     \x20           total += i\n\
     \x20       i += 1\n\
     \x20   return total\n",
    "def a(x: f64) -> f64:\n    return b(x) + 1.0\ndef b(x: f64) -> f64:\n    return x * 2.0\n",
    "def chained(a: i64, b: i64, c: i64) -> bool:\n    return a < b < c\n",
];

/// Whatever comes back, its spans must point inside the source.
fn survives(source: &str) {
    if let Err(diags) = compile(source) {
        for d in diags.iter() {
            assert!(
                d.span.start <= d.span.end,
                "inverted span {:?} for {source:?}",
                d.span
            );
            assert!(
                d.span.end as usize <= source.len(),
                "span {:?} past the end of a {}-byte source {source:?}",
                d.span,
                source.len()
            );
        }
    }
}

#[test]
fn every_prefix_of_a_valid_source_is_survivable() {
    for source in CORPUS {
        for cut in 0..=source.len() {
            if source.is_char_boundary(cut) {
                survives(&source[..cut]);
            }
        }
    }
}

#[test]
fn every_suffix_of_a_valid_source_is_survivable() {
    for source in CORPUS {
        for cut in 0..=source.len() {
            if source.is_char_boundary(cut) {
                survives(&source[cut..]);
            }
        }
    }
}

/// Substitution, deletion, and duplication at every position, cycling through
/// the characters that actually appear in Python source.
#[test]
fn dented_sources_are_survivable() {
    const DENTS: &[char] = &[
        ' ', '\t', '\n', '(', ')', '[', ']', '{', '}', ':', ',', '.', '"', '\'', '\\', '#', '-',
        '+', '*', '/', '%', '=', '<', '>', '@', '~', '&', '|', '^', '0', '9', 'x', '_', 'é', '\0',
    ];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for source in CORPUS {
        let chars: Vec<char> = source.chars().collect();
        for at in 0..chars.len() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let dent = DENTS[(state >> 33) as usize % DENTS.len()];

            let mut substituted = chars.clone();
            substituted[at] = dent;
            survives(&substituted.iter().collect::<String>());

            let mut deleted = chars.clone();
            deleted.remove(at);
            survives(&deleted.iter().collect::<String>());

            let mut inserted = chars.clone();
            inserted.insert(at, dent);
            survives(&inserted.iter().collect::<String>());
        }
    }
}

#[test]
fn degenerate_inputs_are_survivable() {
    for source in [
        "",
        " ",
        "\n",
        "\t\t\t",
        "\0",
        "def",
        "def f",
        "def f(",
        "def f() ->",
        "def f() -> f64:",
        "def f() -> f64:\n",
        "def f() -> f64:\n    ",
        "def f() -> f64:\n    return",
        "return 1.0",
        "1.0",
        "((((",
        "))))",
        "\"unterminated",
        "'''",
        "# just a comment",
        "\u{1F600}",
        "def \u{1F600}() -> f64:\n    return 1.0\n",
        "def f() -> f64:\n\treturn 1.0\n",
        "def f() -> f64:\n  return 1.0\n     return 2.0\n",
        "def f() -> f64:\n    return 0x7fffffffffffffff\n",
        "def f() -> f64:\n    return 1e400\n",
        "def f() -> f64:\n    return .\n",
        "def f() -> f64:\n    return 1..2\n",
        "def f(x: f64, x: f64) -> f64:\n    return x\n",
    ] {
        survives(source);
    }
}

/// Long, but not nested: a flat program of any size is fine, and must not be
/// mistaken for an attack.
#[test]
fn a_very_long_flat_program_still_compiles() {
    let mut source = String::from("def f(x: f64) -> f64:\n    acc = x\n");
    for i in 0..2000 {
        source.push_str(&format!("    acc = acc + {}.0\n", i % 7));
    }
    source.push_str("    return acc\n");
    assert!(compile(&source).is_ok());
}

/// Deep nesting is a diagnostic, not an abort. Every one of these shapes
/// aborted a 2 MiB thread before the parse moved off it.
#[test]
fn deeply_nested_sources_are_diagnostics() {
    let shapes = [
        format!("def f() -> f64:\n    return {}1.0\n", "-".repeat(200_000)),
        format!("def f() -> f64:\n    return 1.0{}\n", "+1.0".repeat(50_000)),
        format!(
            "def g(x: f64) -> f64:\n    return x\ndef f() -> f64:\n    return {}1.0{}\n",
            "g(".repeat(40_000),
            ")".repeat(40_000)
        ),
    ];
    for source in &shapes {
        assert!(source.len() < 256 * 1024, "shape must fit under the cap");
        survives(source);
        assert!(compile(source).is_err(), "deep nesting must be refused");
    }

    // Parentheses group without nesting the AST, so this is merely long.
    let parens = format!(
        "def f() -> f64:\n    return {}1.0{}\n",
        "(".repeat(60_000),
        ")".repeat(60_000)
    );
    survives(&parens);
    assert!(compile(&parens).is_ok());
}

/// Past the size cap the answer is a diagnostic, immediately.
#[test]
fn an_oversized_source_is_refused_by_the_gate() {
    let source = "-".repeat(300_000);
    let diags = compile(&source).unwrap_err();
    assert!(format!("{diags}").contains("byte limit"));
}
