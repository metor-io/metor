//! What a body can name while it is checked.

use std::collections::BTreeSet;

use crate::ast::ProgramKind;
use crate::hir;
use crate::types::Prim;

/// Which body is being checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ctx {
    Def,
    /// A system's state initializers.
    Init,
    /// A system's `loop:`.
    Step,
    /// A sequence's steps, which may wait.
    SeqBody,
    SeqAbort,
    Redline,
}

/// Where a variable lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loc {
    Local(u32),
    /// An offset in the state block.
    State(u32),
}

#[derive(Clone, Debug)]
pub struct Var {
    pub name: String,
    pub prim: Prim,
    pub loc: Loc,
}

/// One port and where its frame sits in its block.
#[derive(Clone, Debug)]
pub struct Port {
    pub name: String,
    pub frame: usize,
    pub offset: u32,
}

/// The program a body belongs to: its ports, params and state.
#[derive(Clone, Debug)]
pub struct Prog {
    pub kind: ProgramKind,
    pub inputs: Vec<Port>,
    pub outputs: Vec<Port>,
    /// Each param's name, type and offset.
    pub params: Vec<(String, Prim, u32)>,
    /// The program's state variables, which every body sees.
    pub state: Vec<Var>,
    pub state_size: u32,
    pub fails: Vec<String>,
    /// How many resume points the body has used.
    pub points: u32,
}

impl Prog {
    pub fn new(kind: ProgramKind) -> Prog {
        Prog {
            kind,
            inputs: Vec::new(),
            outputs: Vec::new(),
            params: Vec::new(),
            state: Vec::new(),
            state_size: 0,
            fails: Vec::new(),
            points: 0,
        }
    }

    /// Reserves `size` bytes of state, aligned to `size`.
    pub fn alloc(&mut self, size: u32) -> u32 {
        let offset = self.state_size.next_multiple_of(size.max(1));
        self.state_size = offset + size;
        offset
    }

    /// Adds a state variable.
    pub fn state_var(&mut self, name: &str, prim: Prim) -> Loc {
        let loc = Loc::State(self.alloc(prim.size()));
        self.state.push(Var {
            name: name.to_string(),
            prim,
            loc,
        });
        loc
    }

    /// Adds a failure message and returns its index.
    pub fn fail(&mut self, message: &str) -> u32 {
        let index = self.fails.len() as u32;
        self.fails.push(message.to_string());
        index
    }

    pub fn next_point(&mut self) -> u32 {
        self.points += 1;
        self.points
    }
}

/// The names one body sees, and what checking it found.
pub struct Scope {
    pub ctx: Ctx,
    pub prog: Prog,
    /// The body's own locals, by name.
    pub vars: Vec<Var>,
    /// The type of each Cranelift local.
    pub locals: Vec<Prim>,
    pub ret: Option<Prim>,
    /// The defs the body calls.
    pub calls: Vec<u32>,
    /// The outputs the body writes.
    pub writes: BTreeSet<u32>,
}

impl Scope {
    /// The scope of a `def`, whose params are its first locals.
    pub fn def(params: &[(String, Prim)], ret: Prim) -> Scope {
        let mut scope = Scope::program(Ctx::Def, Prog::new(ProgramKind::System));
        scope.ret = Some(ret);
        for (name, prim) in params {
            scope.new_var(name, *prim);
        }
        scope
    }

    pub fn program(ctx: Ctx, prog: Prog) -> Scope {
        Scope {
            ctx,
            prog,
            vars: Vec::new(),
            locals: Vec::new(),
            ret: None,
            calls: Vec::new(),
            writes: BTreeSet::new(),
        }
    }

    /// Declares a variable. A sequence's steps keep theirs in state, so
    /// they survive a wait.
    pub fn new_var(&mut self, name: &str, prim: Prim) -> Loc {
        if self.ctx == Ctx::SeqBody {
            return self.prog.state_var(name, prim);
        }
        let loc = Loc::Local(self.new_local(prim));
        self.vars.push(Var {
            name: name.to_string(),
            prim,
            loc,
        });
        loc
    }

    /// Adds an unnamed Cranelift local.
    pub fn new_local(&mut self, prim: Prim) -> u32 {
        self.locals.push(prim);
        (self.locals.len() - 1) as u32
    }

    /// Finds a variable: the body's own, then the program's state.
    pub fn var(&self, name: &str) -> Option<&Var> {
        let own = self.vars.iter().rev().find(|v| v.name == name);
        own.or_else(|| self.prog.state.iter().find(|v| v.name == name))
    }

    pub fn param(&self, name: &str) -> Option<(Prim, u32)> {
        self.prog
            .params
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, prim, offset)| (*prim, *offset))
    }

    /// Finds a port: `Some((true, i))` for input `i`, `Some((false, i))` for
    /// output `i`.
    pub fn port(&self, name: &str) -> Option<(bool, usize)> {
        if let Some(i) = self.prog.inputs.iter().position(|p| p.name == name) {
            return Some((true, i));
        }
        self.prog
            .outputs
            .iter()
            .position(|p| p.name == name)
            .map(|i| (false, i))
    }

    /// Ends the body.
    pub fn finish(&mut self, stmts: Vec<hir::Stmt>) -> hir::Body {
        hir::Body {
            locals: core::mem::take(&mut self.locals),
            stmts,
        }
    }
}
