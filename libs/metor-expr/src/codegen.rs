//! Typed IR to wasm, appended to the prelude.
//!
//! Scalar arithmetic is native opcodes; the prelude is entered only where wasm
//! has no instruction — the transcendentals, and `fmod_floor` for float `%`.
//! Two operators cost more than one instruction, and both for the same reason:
//! wasm's `rem` truncates where Python floors, so `//` and `%` on integers
//! carry a correction that adjusts the result when the operands' signs differ.
//!
//! Tensors never touch the wasm stack. Each one has a buffer this module
//! places above the linker's `__heap_base`, so an elementwise call site is
//! `i32.const desc; call $k_add` with every address and shape in `desc` baked
//! into a data segment. That is what "compile-time-constant shapes" and "no
//! guest allocator" both cash out to.
//!
//! Nothing here re-checks anything. The IR arrived from a single validation
//! gate, so a `Call` index is in range, an operand type is already unified, and
//! a condition is already `bool`.

use std::collections::HashMap;

use wasm_encoder::{BlockType, Function, Instruction, ValType};

use crate::ir::{Arith, BufId, Cmp, Desc, Expr, Func, Intrinsic, Num, Program, Stmt};
use crate::template::Template;
use crate::{Manifest, PRELUDE, Ty};

/// Bytes an elementwise descriptor occupies: three addresses, a rank, and
/// three shapes of four axes.
const DESC_BYTES: u32 = 16 * 4;

pub(crate) fn emit(program: &Program, manifest: &Manifest) -> Vec<u8> {
    let kernels = program.kernels();
    let plan = Template::parse(PRELUDE)
        .expect("the checked-in prelude parses")
        .plan(&kernels)
        .expect("the checked-in prelude exports every kernel the IR names");
    let mut splice = plan.splice();

    let kernel_index: HashMap<&'static str, u32> =
        kernels.iter().map(|k| (*k, splice.kernel(k))).collect();
    let base = splice.next_function();

    // Buffers first, then the descriptors kernel call sites point at, all
    // eight-aligned so an f64 load never straddles.
    let mut cursor = (splice.heap_base() + 7) & !7;
    let addresses: Vec<u32> = program
        .buffers
        .iter()
        .map(|bytes| {
            let at = cursor;
            cursor += (bytes + 7) & !7;
            at
        })
        .collect();

    let layout = Layout {
        addresses,
        descriptors: cursor,
    };

    let types: Vec<u32> = manifest
        .functions
        .iter()
        .zip(&program.funcs)
        .map(|(sig, func)| {
            if func.buffer_abi {
                splice.ty(vec![], vec![ValType::I32])
            } else {
                let params: Vec<ValType> = sig.params.iter().map(|(_, t)| val_type(t)).collect();
                splice.ty(params, vec![val_type(&sig.ret)])
            }
        })
        .collect();

    let mut descriptors: Vec<u8> = Vec::new();
    let mut indices = Vec::with_capacity(program.funcs.len());
    for (func, ty) in program.funcs.iter().zip(&types) {
        let body = Emitter::new(func, program, &kernel_index, base, &layout, &mut descriptors)
            .finish(func);
        indices.push(splice.function(*ty, body));
    }

    let accessor = splice.ty(vec![ValType::I32], vec![ValType::I32]);
    let nullary = splice.ty(vec![], vec![ValType::I32]);
    for (func, index) in program.funcs.iter().zip(&indices) {
        splice.export(func.name.clone(), *index);
        if !func.buffer_abi {
            continue;
        }
        // `<name>_arg_ptr(i)` is a jump table of constants: the shapes are
        // static, so the addresses are too.
        let mut table = Function::new([]);
        for (i, buf) in func.arg_buffers.iter().enumerate() {
            table.instruction(&Instruction::LocalGet(0));
            table.instruction(&Instruction::I32Const(i as i32));
            table.instruction(&Instruction::I32Eq);
            table.instruction(&Instruction::If(BlockType::Empty));
            table.instruction(&Instruction::I32Const(layout.at(*buf) as i32));
            table.instruction(&Instruction::Return);
            table.instruction(&Instruction::End);
        }
        table.instruction(&Instruction::I32Const(0));
        table.instruction(&Instruction::End);
        let arg_ptr = splice.function(accessor, table);
        splice.export(format!("{}_arg_ptr", func.name), arg_ptr);

        let mut ret = Function::new([]);
        ret.instruction(&Instruction::I32Const(
            layout.at(func.ret_buffer.expect("a buffer-ABI function has one")) as i32,
        ));
        ret.instruction(&Instruction::End);
        let ret_ptr = splice.function(nullary, ret);
        splice.export(format!("{}_ret_ptr", func.name), ret_ptr);
    }

    if !descriptors.is_empty() {
        splice.data(layout.descriptors, descriptors.clone());
    }
    splice.reserve_memory(layout.descriptors + descriptors.len() as u32);

    splice.finish()
}

struct Layout {
    addresses: Vec<u32>,
    descriptors: u32,
}

impl Layout {
    fn at(&self, buf: BufId) -> u32 {
        self.addresses[buf as usize]
    }
}

pub(crate) fn val_type(ty: &Ty) -> ValType {
    match ty {
        Ty::F64 => ValType::F64,
        Ty::I64 => ValType::I64,
        Ty::Bool => ValType::I32,
        Ty::Tensor { .. } => ValType::I32,
    }
}

struct Emitter<'a> {
    locals: Vec<ValType>,
    param_count: usize,
    free: HashMap<ValType, Vec<u32>>,
    code: Vec<Instruction<'static>>,
    depth: u32,
    loops: Vec<(u32, u32)>,
    kernels: &'a HashMap<&'static str, u32>,
    base: u32,
    program: &'a Program,
    layout: &'a Layout,
    descriptors: &'a mut Vec<u8>,
}

impl<'a> Emitter<'a> {
    fn new(
        func: &Func,
        program: &'a Program,
        kernels: &'a HashMap<&'static str, u32>,
        base: u32,
        layout: &'a Layout,
        descriptors: &'a mut Vec<u8>,
    ) -> Self {
        Emitter {
            locals: func.locals.iter().map(val_type).collect(),
            param_count: func.param_count,
            free: HashMap::new(),
            code: Vec::new(),
            depth: 0,
            loops: Vec::new(),
            kernels,
            base,
            program,
            layout,
            descriptors,
        }
    }

    fn finish(mut self, func: &Func) -> Function {
        for (buf, slot, ty) in &func.prologue {
            self.push(Instruction::I32Const(self.layout.at(*buf) as i32));
            self.push(load_of(ty));
            self.push(Instruction::LocalSet(*slot));
        }
        self.block(&func.body);
        if func.buffer_abi {
            self.code.push(Instruction::I32Const(0));
            self.code.push(Instruction::Return);
        }
        self.code.push(Instruction::Unreachable);
        self.code.push(Instruction::End);

        let mut runs: Vec<(u32, ValType)> = Vec::new();
        for ty in &self.locals[self.param_count..] {
            match runs.last_mut() {
                Some((count, last)) if last == ty => *count += 1,
                _ => runs.push((1, *ty)),
            }
        }
        let mut body = Function::new(runs);
        for instruction in &self.code {
            body.instruction(instruction);
        }
        body
    }

    fn acquire(&mut self, ty: ValType) -> u32 {
        match self.free.get_mut(&ty).and_then(Vec::pop) {
            Some(slot) => slot,
            None => {
                self.locals.push(ty);
                (self.locals.len() - 1) as u32
            }
        }
    }

    fn release(&mut self, ty: ValType, slot: u32) {
        self.free.entry(ty).or_default().push(slot);
    }

    fn push(&mut self, instruction: Instruction<'static>) {
        self.code.push(instruction);
    }

    fn block(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign { local, value } => {
                self.expr(value);
                self.push(Instruction::LocalSet(*local));
            }
            Stmt::Return(value) => {
                self.expr(value);
                self.push(Instruction::Return);
            }
            Stmt::Drop(value) => {
                self.expr(value);
                self.push(Instruction::Drop);
            }
            Stmt::If { cond, then, els } => {
                self.expr(cond);
                self.push(Instruction::If(BlockType::Empty));
                self.depth += 1;
                self.block(then);
                if !els.is_empty() {
                    self.push(Instruction::Else);
                    self.block(els);
                }
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Stmt::While { cond, body } => {
                self.push(Instruction::Block(BlockType::Empty));
                self.depth += 1;
                let break_frame = self.depth;
                self.push(Instruction::Loop(BlockType::Empty));
                self.depth += 1;
                let continue_frame = self.depth;
                self.loops.push((break_frame, continue_frame));

                self.expr(cond);
                self.push(Instruction::I32Eqz);
                let out = self.depth - break_frame;
                self.push(Instruction::BrIf(out));
                self.block(body);
                let back = self.depth - continue_frame;
                self.push(Instruction::Br(back));

                self.loops.pop();
                self.depth -= 1;
                self.push(Instruction::End);
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Stmt::TensorAssign { dest, value, bytes } => {
                self.expr(value);
                if *bytes > 0 {
                    self.copy(self.layout.at(*dest), value.buffer(), *bytes);
                }
            }
            Stmt::ElementAssign {
                target,
                index,
                value,
                len,
            } => {
                self.element_address(target, index, *len);
                self.expr(value);
                self.push(Instruction::F64Store(mem_arg(3)));
            }
            Stmt::ReturnBuffer { dest, value, ty } => {
                self.expr(value);
                match ty {
                    Ty::Tensor { .. } => {
                        self.copy(self.layout.at(*dest), value.buffer(), slot_bytes(ty))
                    }
                    _ => {
                        let slot = self.acquire(val_type(ty));
                        self.push(Instruction::LocalSet(slot));
                        self.push(Instruction::I32Const(self.layout.at(*dest) as i32));
                        self.push(Instruction::LocalGet(slot));
                        self.push(store_of(ty));
                        self.release(val_type(ty), slot);
                    }
                }
                self.push(Instruction::I32Const(0));
                self.push(Instruction::Return);
            }
            Stmt::Break => {
                let (frame, _) = *self.loops.last().expect("the checker rejects a bare break");
                let target = self.depth - frame;
                self.push(Instruction::Br(target));
            }
            Stmt::Continue => {
                let (_, frame) = *self
                    .loops
                    .last()
                    .expect("the checker rejects a bare continue");
                let target = self.depth - frame;
                self.push(Instruction::Br(target));
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::F64(v) => self.push(Instruction::F64Const((*v).into())),
            Expr::I64(v) => self.push(Instruction::I64Const(*v)),
            Expr::Bool(v) => self.push(Instruction::I32Const(*v as i32)),
            Expr::Local(slot) => self.push(Instruction::LocalGet(*slot)),
            Expr::Store { local, value, then } => {
                self.expr(value);
                self.push(Instruction::LocalSet(*local));
                self.expr(then);
            }
            Expr::Arith { op, ty, lhs, rhs } => self.arith(*op, *ty, lhs, rhs),
            Expr::PowConst { ty, base, exp } => self.pow_const(*ty, base, *exp),
            Expr::Cmp { op, ty, lhs, rhs } => {
                self.expr(lhs);
                self.expr(rhs);
                self.push(compare(*op, *ty));
            }
            Expr::CmpBool { eq, lhs, rhs } => {
                self.expr(lhs);
                self.expr(rhs);
                self.push(if *eq {
                    Instruction::I32Eq
                } else {
                    Instruction::I32Ne
                });
            }
            Expr::Not(inner) => {
                self.expr(inner);
                self.push(Instruction::I32Eqz);
            }
            Expr::And(lhs, rhs) => {
                self.expr(lhs);
                self.push(Instruction::If(BlockType::Result(ValType::I32)));
                self.depth += 1;
                self.expr(rhs);
                self.push(Instruction::Else);
                self.push(Instruction::I32Const(0));
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Expr::Or(lhs, rhs) => {
                self.expr(lhs);
                self.push(Instruction::If(BlockType::Result(ValType::I32)));
                self.depth += 1;
                self.push(Instruction::I32Const(1));
                self.push(Instruction::Else);
                self.expr(rhs);
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Expr::Select {
                cond,
                then,
                els,
                ty,
            } => {
                self.expr(cond);
                self.push(Instruction::If(BlockType::Result(val_type(ty))));
                self.depth += 1;
                self.expr(then);
                self.push(Instruction::Else);
                self.expr(els);
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Expr::Call { index, args, .. } => {
                for arg in args {
                    self.expr(arg);
                }
                self.push(Instruction::Call(self.base + index));
            }
            Expr::Kernel { name, args, .. } => {
                for arg in args {
                    self.expr(arg);
                }
                self.push(Instruction::Call(self.kernels[name]));
            }
            Expr::Intrinsic { op, args } => self.intrinsic(*op, args),

            // A tensor is already where it belongs; naming one costs nothing.
            Expr::Tensor(_) => {}
            Expr::Elementwise {
                kernel,
                dest,
                lhs,
                rhs,
                desc,
            } => {
                self.expr(lhs);
                self.expr(rhs);
                let at = self.descriptor(lhs.buffer(), rhs.buffer(), *dest, desc);
                self.push(Instruction::I32Const(at as i32));
                self.push(Instruction::Call(self.kernels[kernel]));
            }
            Expr::Splat { dest, value } => {
                self.push(Instruction::I32Const(self.layout.at(*dest) as i32));
                self.expr(value);
                self.push(Instruction::F64Store(mem_arg(3)));
            }
            Expr::TensorNeg {
                dest,
                operand,
                elems,
            } => {
                self.expr(operand);
                self.push(Instruction::I32Const(self.layout.at(operand.buffer()) as i32));
                self.push(Instruction::I32Const(self.layout.at(*dest) as i32));
                self.push(Instruction::I32Const(*elems as i32));
                self.push(Instruction::Call(self.kernels["k_neg"]));
            }
            Expr::Element { source, index, len } => {
                self.element_address(source, index, *len);
                self.push(Instruction::F64Load(mem_arg(3)));
            }
            Expr::Dot { lhs, rhs, len } => {
                self.expr(lhs);
                self.expr(rhs);
                self.push(Instruction::I32Const(self.layout.at(lhs.buffer()) as i32));
                self.push(Instruction::I32Const(self.layout.at(rhs.buffer()) as i32));
                self.push(Instruction::I32Const(*len as i32));
                self.push(Instruction::Call(self.kernels["k_dot"]));
            }
            Expr::MatMul {
                dest,
                lhs,
                rhs,
                m,
                k,
                n,
            } => {
                self.expr(lhs);
                self.expr(rhs);
                self.push(Instruction::I32Const(self.layout.at(lhs.buffer()) as i32));
                self.push(Instruction::I32Const(self.layout.at(rhs.buffer()) as i32));
                self.push(Instruction::I32Const(self.layout.at(*dest) as i32));
                for extent in [m, k, n] {
                    self.push(Instruction::I32Const(*extent as i32));
                }
                self.push(Instruction::Call(self.kernels["k_matmul"]));
            }
            Expr::Sum { source, elems } => {
                self.expr(source);
                self.push(Instruction::I32Const(self.layout.at(source.buffer()) as i32));
                self.push(Instruction::I32Const(*elems as i32));
                self.push(Instruction::Call(self.kernels["k_sum"]));
            }
            Expr::BufferCall {
                index,
                args,
                dest,
                ret,
            } => {
                let callee = &self.program.funcs[*index as usize];
                let params = &self.program.param_types[*index as usize];
                for ((arg, buf), ty) in args.iter().zip(&callee.arg_buffers).zip(params) {
                    self.expr(arg);
                    match ty {
                        Ty::Tensor { .. } => {
                            self.copy(self.layout.at(*buf), arg.buffer(), slot_bytes(ty))
                        }
                        _ => {
                            let slot = self.acquire(val_type(ty));
                            self.push(Instruction::LocalSet(slot));
                            self.push(Instruction::I32Const(self.layout.at(*buf) as i32));
                            self.push(Instruction::LocalGet(slot));
                            self.push(store_of(ty));
                            self.release(val_type(ty), slot);
                        }
                    }
                }
                self.push(Instruction::Call(self.base + index));
                self.push(Instruction::Drop);
                let ret_buffer = callee.ret_buffer.expect("a buffer-ABI function has one");
                match (ret, dest) {
                    // The callee's result buffer is reused by its next call,
                    // so the caller takes a copy it owns.
                    (Ty::Tensor { .. }, Some(dest)) => {
                        self.copy(self.layout.at(*dest), ret_buffer, slot_bytes(ret))
                    }
                    _ => {
                        self.push(Instruction::I32Const(self.layout.at(ret_buffer) as i32));
                        self.push(load_of(ret));
                    }
                }
            }
        }
    }

    /// `dest` and `source` are both constants, so a copy is three constants
    /// and one instruction.
    fn copy(&mut self, dest: u32, source: BufId, bytes: u32) {
        self.push(Instruction::I32Const(dest as i32));
        self.push(Instruction::I32Const(self.layout.at(source) as i32));
        self.push(Instruction::I32Const(bytes as i32));
        self.push(Instruction::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        });
    }

    /// Leaves the address of element `index` of `source` on the stack, after
    /// trapping if the index is out of range. An unsigned compare catches
    /// negatives too, which is why `v[-1]` traps rather than wrapping around.
    fn element_address(&mut self, source: &Expr, index: &Expr, len: u32) {
        self.expr(source);
        let base = self.layout.at(source.buffer());
        if let Expr::I64(constant) = index {
            self.push(Instruction::I32Const(base as i32 + *constant as i32 * 8));
            return;
        }
        let slot = self.acquire(ValType::I64);
        self.expr(index);
        self.push(Instruction::LocalTee(slot));
        self.push(Instruction::I64Const(len as i64));
        self.push(Instruction::I64GeU);
        self.push(Instruction::If(BlockType::Empty));
        self.depth += 1;
        self.push(Instruction::Unreachable);
        self.depth -= 1;
        self.push(Instruction::End);
        self.push(Instruction::LocalGet(slot));
        self.push(Instruction::I32WrapI64);
        self.push(Instruction::I32Const(8));
        self.push(Instruction::I32Mul);
        self.push(Instruction::I32Const(base as i32));
        self.push(Instruction::I32Add);
        self.release(ValType::I64, slot);
    }

    /// Intern a kernel descriptor, so two identical call sites share one.
    fn descriptor(&mut self, lhs: BufId, rhs: BufId, dest: BufId, desc: &Desc) -> u32 {
        let mut bytes = Vec::with_capacity(DESC_BYTES as usize);
        for word in [
            self.layout.at(lhs),
            self.layout.at(rhs),
            self.layout.at(dest),
            desc.rank,
        ] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        for shape in [&desc.lhs, &desc.rhs, &desc.out] {
            for axis in 0..4 {
                let extent = shape.get(axis).copied().unwrap_or(0);
                bytes.extend_from_slice(&extent.to_le_bytes());
            }
        }
        if let Some(at) = self
            .descriptors
            .chunks_exact(DESC_BYTES as usize)
            .position(|existing| existing == bytes)
        {
            return self.layout.descriptors + at as u32 * DESC_BYTES;
        }
        let at = self.layout.descriptors + self.descriptors.len() as u32;
        self.descriptors.extend_from_slice(&bytes);
        at
    }

    fn arith(&mut self, op: Arith, ty: Num, lhs: &Expr, rhs: &Expr) {
        match (op, ty) {
            (Arith::Add, Num::F64) => self.simple(lhs, rhs, Instruction::F64Add),
            (Arith::Add, Num::I64) => self.simple(lhs, rhs, Instruction::I64Add),
            (Arith::Sub, Num::F64) => self.simple(lhs, rhs, Instruction::F64Sub),
            (Arith::Sub, Num::I64) => self.simple(lhs, rhs, Instruction::I64Sub),
            (Arith::Mul, Num::F64) => self.simple(lhs, rhs, Instruction::F64Mul),
            (Arith::Mul, Num::I64) => self.simple(lhs, rhs, Instruction::I64Mul),
            (Arith::Div, _) => self.simple(lhs, rhs, Instruction::F64Div),
            (Arith::FloorDiv, Num::F64) => {
                self.simple(lhs, rhs, Instruction::F64Div);
                self.push(Instruction::F64Floor);
            }
            (Arith::FloorDiv, Num::I64) => self.floor_div_i64(lhs, rhs),
            (Arith::Rem, Num::I64) => self.floor_rem_i64(lhs, rhs),
            (Arith::Rem, Num::F64) => unreachable!("float `%` lowers to the fmod_floor kernel"),
        }
    }

    fn simple(&mut self, lhs: &Expr, rhs: &Expr, op: Instruction<'static>) {
        self.expr(lhs);
        self.expr(rhs);
        self.push(op);
    }

    /// `a // b` with Python's sign rule: `i64.div_s` truncates, so subtract one
    /// when the remainder is non-zero and the operands disagree in sign.
    fn floor_div_i64(&mut self, lhs: &Expr, rhs: &Expr) {
        let (a, b, r) = (
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
        );
        self.expr(lhs);
        self.push(Instruction::LocalSet(a));
        self.expr(rhs);
        self.push(Instruction::LocalSet(b));

        self.push(Instruction::LocalGet(a));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64DivS);

        self.push(Instruction::LocalGet(a));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64RemS);
        self.push(Instruction::LocalSet(r));
        self.signs_differ(b, r);
        self.push(Instruction::I64ExtendI32U);
        self.push(Instruction::I64Sub);

        self.release(ValType::I64, a);
        self.release(ValType::I64, b);
        self.release(ValType::I64, r);
    }

    /// `a % b` with Python's sign rule: add the divisor back when the
    /// truncating remainder landed on the wrong side of zero.
    fn floor_rem_i64(&mut self, lhs: &Expr, rhs: &Expr) {
        let (a, b, r) = (
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
        );
        self.expr(lhs);
        self.push(Instruction::LocalSet(a));
        self.expr(rhs);
        self.push(Instruction::LocalSet(b));

        self.push(Instruction::LocalGet(a));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64RemS);
        self.push(Instruction::LocalTee(r));

        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64Const(0));
        self.signs_differ(b, r);
        self.push(Instruction::Select);
        self.push(Instruction::I64Add);

        self.release(ValType::I64, a);
        self.release(ValType::I64, b);
        self.release(ValType::I64, r);
    }

    /// Leaves `1` on the stack when the remainder in `r` is non-zero and its
    /// sign disagrees with the divisor in `b`.
    fn signs_differ(&mut self, b: u32, r: u32) {
        self.push(Instruction::LocalGet(r));
        self.push(Instruction::I64Eqz);
        self.push(Instruction::I32Eqz);
        self.push(Instruction::LocalGet(r));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64Xor);
        self.push(Instruction::I64Const(0));
        self.push(Instruction::I64LtS);
        self.push(Instruction::I32And);
    }

    fn pow_const(&mut self, ty: Num, base: &Expr, exp: u32) {
        let one = match ty {
            Num::F64 => Instruction::F64Const(1.0.into()),
            Num::I64 => Instruction::I64Const(1),
        };
        if exp == 0 {
            self.push(one);
            return;
        }
        if exp == 1 {
            self.expr(base);
            return;
        }
        let slot = self.acquire(val_type_of(ty));
        self.expr(base);
        self.push(Instruction::LocalSet(slot));
        self.push(Instruction::LocalGet(slot));
        for _ in 1..exp {
            self.push(Instruction::LocalGet(slot));
            self.push(match ty {
                Num::F64 => Instruction::F64Mul,
                Num::I64 => Instruction::I64Mul,
            });
        }
        self.release(val_type_of(ty), slot);
    }

    fn intrinsic(&mut self, op: Intrinsic, args: &[Expr]) {
        match op {
            Intrinsic::NegF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Neg);
            }
            Intrinsic::NegI64 => {
                self.push(Instruction::I64Const(0));
                self.expr(&args[0]);
                self.push(Instruction::I64Sub);
            }
            Intrinsic::AbsF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Abs);
            }
            Intrinsic::AbsI64 => {
                let slot = self.acquire(ValType::I64);
                self.expr(&args[0]);
                self.push(Instruction::LocalSet(slot));
                self.push(Instruction::LocalGet(slot));
                self.push(Instruction::I64Const(0));
                self.push(Instruction::LocalGet(slot));
                self.push(Instruction::I64Sub);
                self.push(Instruction::LocalGet(slot));
                self.push(Instruction::I64Const(0));
                self.push(Instruction::I64GeS);
                self.push(Instruction::Select);
                self.release(ValType::I64, slot);
            }
            Intrinsic::SqrtF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Sqrt);
            }
            Intrinsic::FloorF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Floor);
            }
            Intrinsic::CeilF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Ceil);
            }
            Intrinsic::TruncF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Trunc);
            }
            Intrinsic::NearestF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Nearest);
            }
            Intrinsic::MinF64 => self.simple(&args[0], &args[1], Instruction::F64Min),
            Intrinsic::MaxF64 => self.simple(&args[0], &args[1], Instruction::F64Max),
            Intrinsic::MinI64 | Intrinsic::MaxI64 => {
                let (a, b) = (self.acquire(ValType::I64), self.acquire(ValType::I64));
                self.expr(&args[0]);
                self.push(Instruction::LocalSet(a));
                self.expr(&args[1]);
                self.push(Instruction::LocalSet(b));
                self.push(Instruction::LocalGet(a));
                self.push(Instruction::LocalGet(b));
                self.push(Instruction::LocalGet(a));
                self.push(Instruction::LocalGet(b));
                self.push(if op == Intrinsic::MinI64 {
                    Instruction::I64LtS
                } else {
                    Instruction::I64GtS
                });
                self.push(Instruction::Select);
                self.release(ValType::I64, a);
                self.release(ValType::I64, b);
            }
            Intrinsic::IntToFloat => {
                self.expr(&args[0]);
                self.push(Instruction::F64ConvertI64S);
            }
            Intrinsic::FloatToInt => {
                self.expr(&args[0]);
                self.push(Instruction::I64TruncF64S);
            }
        }
    }
}

fn val_type_of(ty: Num) -> ValType {
    match ty {
        Num::F64 => ValType::F64,
        Num::I64 => ValType::I64,
    }
}

fn compare(op: Cmp, ty: Num) -> Instruction<'static> {
    match (op, ty) {
        (Cmp::Eq, Num::F64) => Instruction::F64Eq,
        (Cmp::Ne, Num::F64) => Instruction::F64Ne,
        (Cmp::Lt, Num::F64) => Instruction::F64Lt,
        (Cmp::Le, Num::F64) => Instruction::F64Le,
        (Cmp::Gt, Num::F64) => Instruction::F64Gt,
        (Cmp::Ge, Num::F64) => Instruction::F64Ge,
        (Cmp::Eq, Num::I64) => Instruction::I64Eq,
        (Cmp::Ne, Num::I64) => Instruction::I64Ne,
        (Cmp::Lt, Num::I64) => Instruction::I64LtS,
        (Cmp::Le, Num::I64) => Instruction::I64LeS,
        (Cmp::Gt, Num::I64) => Instruction::I64GtS,
        (Cmp::Ge, Num::I64) => Instruction::I64GeS,
    }
}

fn mem_arg(align: u32) -> wasm_encoder::MemArg {
    wasm_encoder::MemArg {
        offset: 0,
        align,
        memory_index: 0,
    }
}

fn load_of(ty: &Ty) -> Instruction<'static> {
    match ty {
        Ty::F64 => Instruction::F64Load(mem_arg(3)),
        Ty::I64 => Instruction::I64Load(mem_arg(3)),
        Ty::Bool => Instruction::I32Load(mem_arg(2)),
        Ty::Tensor { .. } => unreachable!("tensors are addressed, not loaded"),
    }
}

fn store_of(ty: &Ty) -> Instruction<'static> {
    match ty {
        Ty::F64 => Instruction::F64Store(mem_arg(3)),
        Ty::I64 => Instruction::I64Store(mem_arg(3)),
        Ty::Bool => Instruction::I32Store(mem_arg(2)),
        Ty::Tensor { .. } => unreachable!("tensors are copied, not stored"),
    }
}

fn slot_bytes(ty: &Ty) -> u32 {
    match ty {
        Ty::Tensor { shape, .. } => (shape.iter().product::<usize>() as u32) * 8,
        _ => 8,
    }
}
