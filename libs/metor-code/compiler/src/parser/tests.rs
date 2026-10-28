use super::*;
use crate::lexer::lex;

fn parse_ok(src: &str) -> File {
    let (tokens, lex_errors) = lex(src);
    assert!(lex_errors.is_empty(), "{lex_errors:?}");
    let (file, errors) = parse(&tokens, src.len());
    assert!(errors.is_empty(), "{errors:#?}");
    file.expect("a file")
}

fn parse_errors(src: &str) -> Vec<Diagnostic> {
    let (tokens, _) = lex(src);
    parse(&tokens, src.len()).1
}

fn program(file: &File, index: usize) -> &ProgramDecl {
    match &file.items[index] {
        Item::Program(program) => program,
        other => panic!("not a program: {other:?}"),
    }
}

const HEATER: &str = r#"
use frames from "stand_io"

frame HeaterCmd:
    duty: f64
    gains: [f32; 3]

const KP = 0.5

def sq(x: f64) -> f64:
    return x * x

system heater(setpoint = 40.0, limit: f64 = 1.0):
    "Holds the plate at its setpoint."
    in temp: Thermal
    out heater: HeaterCmd
    integ = 0.0
    alert overtemp critical: temp.c > 80.0 for 2.0
    loop:
        err = setpoint - temp.c
        integ = clamp(integ + err * dt, -10.0, 10.0)
        heater.duty = clamp(KP * err + 0.01 * integ, 0.0, limit)
        for i in range(3):
            heater.gains[i] = 1.0
"#;

#[test]
fn test_parses_system() {
    let file = parse_ok(HEATER);
    assert_eq!(file.items.len(), 5);
    assert!(matches!(&file.items[0], Item::Use { pack, .. } if pack == "stand_io"));
    let Item::Frame(frame) = &file.items[1] else {
        panic!("a frame")
    };
    assert_eq!(frame.fields.len(), 2);
    assert!(matches!(frame.fields[1].ty, TypeExpr::Array { .. }));
    let heater = program(&file, 4);
    assert_eq!(heater.kind, ProgramKind::System);
    assert_eq!(heater.params.len(), 2);
    assert!(matches!(&heater.members[0], Member::Doc(doc) if doc.starts_with("Holds")));
    assert!(matches!(
        &heater.members[4],
        Member::Alert { hold: Some(_), .. }
    ));
    let Member::Loop { body, .. } = &heater.members[5] else {
        panic!("a loop")
    };
    assert_eq!(body.len(), 4);
    assert!(matches!(body[3].kind, StmtKind::For { .. }));
}

const LEAK: &str = r#"
sequence leak_check(target = 150.0):
    in chamber: Chamber
    out valve: ValveCmd

    redline "chamber over": chamber.psi > 165.0
    on abort:
        valve.open = False

    valve = ValveCmd(open = True)
    wait until chamber.psi >= target timeout 30.0
    valve.open = False
    wait 5 min
    if chamber.psi < target - 2.0:
        fail "leak"
    elif chamber.psi > target:
        pass
    else:
        wait 500 ms
"#;

#[test]
fn test_parses_sequence() {
    let file = parse_ok(LEAK);
    let leak = program(&file, 0);
    assert_eq!(leak.kind, ProgramKind::Sequence);
    assert!(matches!(&leak.members[2], Member::Redline { name, .. } if name == "chamber over"));
    assert!(matches!(&leak.members[3], Member::OnAbort { body, .. } if body.len() == 1));
    let Member::Stmt(wait) = &leak.members[5] else {
        panic!("a statement")
    };
    assert!(matches!(
        wait.kind,
        StmtKind::WaitUntil {
            timeout: Some(_),
            ..
        }
    ));
    let Member::Stmt(sleep) = &leak.members[7] else {
        panic!("a statement")
    };
    assert!(matches!(
        sleep.kind,
        StmtKind::Wait(Expr {
            kind: ExprKind::Duration(300.0),
            ..
        })
    ));
    let Member::Stmt(Stmt {
        kind: StmtKind::If { branches, orelse },
        ..
    }) = &leak.members[8]
    else {
        panic!("an if")
    };
    assert_eq!(branches.len(), 2);
    assert!(orelse.is_some());
}

fn expr_of(src: &str) -> Expr {
    let file = parse_ok(&format!("const X = {src}\n"));
    match &file.items[0] {
        Item::Const(c) => c.value.clone(),
        _ => panic!("a const"),
    }
}

fn shape(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Int(v) => v.to_string(),
        ExprKind::Name(n) => n.clone(),
        ExprKind::Unary(op, x) => format!("({op:?} {})", shape(x)),
        ExprKind::Binary(op, a, b) => format!("({} {op:?} {})", shape(a), shape(b)),
        ExprKind::Field(base, f) => format!("{}.{}", shape(base), f.name),
        ExprKind::Index(base, i) => format!("{}[{}]", shape(base), shape(i)),
        ExprKind::Call { callee, args } => format!("{}/{}", callee.name, args.len()),
        other => format!("{other:?}"),
    }
}

#[test]
fn test_precedence() {
    assert_eq!(shape(&expr_of("1 + 2 * 3")), "(1 Add (2 Mul 3))");
    assert_eq!(shape(&expr_of("-a ** 2")), "(Neg (a Pow 2))");
    assert_eq!(
        shape(&expr_of("not a == b and c")),
        "((Not (a Eq b)) And c)"
    );
    assert_eq!(shape(&expr_of("a or b and c")), "(a Or (b And c))");
    assert_eq!(shape(&expr_of("x.y[i].z - f(1, 2)")), "(x.y[i].z Sub f/2)");
    assert_eq!(shape(&expr_of("(1 + 2) * 3")), "((1 Add 2) Mul 3)");
}

#[test]
fn test_machine_is_skipped() {
    let file =
        parse_ok("machine m:\n    state a:\n        loop:\n            goto b\nconst X = 1\n");
    assert!(matches!(&file.items[0], Item::Machine { .. }));
    assert!(matches!(&file.items[1], Item::Const(_)));
}

#[test]
fn test_recovers_and_reports_several_errors() {
    let src = "system a:\n    in x Thermal\n    loop:\n        y = 1 +\n        z = 2\nframe F:\n    a f64\n";
    let errors = parse_errors(src);
    assert!(errors.len() >= 3, "{errors:#?}");
    assert!(errors[0].message.contains("`:` or `=`"), "{errors:#?}");
    let src = "system a:\n    loop:\n        x = = 1\n        y = 2\n";
    let errors = parse_errors(src);
    assert_eq!(errors.len(), 1, "{errors:#?}");
}

#[test]
fn test_error_names_what_was_expected() {
    let errors = parse_errors("frame F:\n    a: \n");
    assert!(errors[0].message.contains("expected"), "{errors:#?}");
}
