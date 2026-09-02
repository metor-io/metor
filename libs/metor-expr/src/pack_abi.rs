//! Generating the `fsw_pack_*` entry points into a compiled module.
//!
//! The pack ABI's guest half splits two ways. The parts that are the same
//! for every module — the allocator, ring formatting, the ring view/writer
//! calls — live in the prelude as real Rust over the linked ring crate
//! ([`PACK_HELPERS`] keeps them through the call-graph GC). The parts that
//! depend on the compiled systems — describe, create, bind, execute — are
//! generated here as straight-line wasm over compile-time constants, exactly
//! like every other body this crate emits: the instance blocks, record
//! staging buffers, and the baked `PackManifest` all live at addresses
//! chosen below the raised memory minimum, and the only runtime value that
//! flows through the unpack code is the borrowed record's base pointer.
//!
//! One instance per entry. The expr backend keeps one set of argument and
//! state buffers per system, so a second concurrent create of the same entry
//! would alias them; `fsw_pack_create` refuses it (returns null) instead.
//! `fsw_pack_destroy` closes the ring handles and clears the block, so a
//! fresh create is legal afterwards — the occupant-reload shape.
//!
//! No panicking paths: every generated body either returns a status the host
//! folds ([`FswStatus::Panicked`] for an unbound or corrupt instance) or
//! silently ignores what the native ABI ignores (a full output ring drops
//! the sample). A guest abort would be invisible — no imports, no message —
//! so nothing here can reach one.
//!
//! [`FswStatus::Panicked`]: metor_fsw_2_core::abi::FswStatus::Panicked

use wasm_encoder::{BlockType, Function, Instruction, MemArg, ValType};

use metor_proto::types::PrimType;

use crate::ir::Program;
use crate::pack::{Fill, PackPlan, SlotTy};
use crate::template::Splice;

/// The prelude's pack surface, rooted through the call-graph GC whenever a
/// pack module is emitted. The first block is called from generated code;
/// the `fsw_*` tail is exported directly by the prelude and only needs to
/// survive.
pub(crate) const PACK_HELPERS: &[&str] = &[
    "pk_view_open",
    "pk_writer_open",
    "pk_view_committed",
    "pk_view_latest",
    "pk_view_close",
    "pk_writer_close",
    "pk_write",
    "fsw_pack_alloc",
    "fsw_pack_ring_init",
    "fsw_pack_set_now",
];

/// Instance-block field offsets. One block per entry, zero-initialized by
/// `fsw_pack_create`, so a module needs no data segment for them.
const CREATED: u32 = 0;
const BOUND: u32 = 4;
/// The `rate=` countdown: fire at zero, reload to `cycles - 1`.
const COUNTDOWN: u32 = 8;
/// Bitmask of groups that have delivered at least one record.
const SEEN: u32 = 16;
/// First group cell; each is 16 bytes: the view handle, then the held
/// committed position.
const GROUPS: u32 = 24;
const GROUP_STRIDE: u32 = 16;
const GROUP_LAST: u32 = 8;

/// Where one entry's generated code keeps its constants.
struct EntryLayout {
    block: u32,
    /// The staging record buffer: 8-byte timestamp then the output frame.
    rec: u32,
    /// `<name>_eval`'s final function index.
    eval: u32,
    /// Argument-frame base address per parameter.
    args: Vec<u32>,
    /// The output frame's buffer (the record's post-timestamp bytes).
    ret: u32,
    /// The `@rng` state slot's address, when the entry draws.
    rng: Option<u32>,
    /// The seed guard's address, when the entry holds state.
    guard: Option<u32>,
}

impl EntryLayout {
    fn writer(&self, n_groups: u32) -> u32 {
        self.block + GROUPS + n_groups * GROUP_STRIDE
    }

    fn view(&self, g: u32) -> u32 {
        self.block + GROUPS + g * GROUP_STRIDE
    }

    fn last(&self, g: u32) -> u32 {
        self.block + GROUPS + g * GROUP_STRIDE + GROUP_LAST
    }
}

/// Append the pack ABI to a spliced module. `end` is the first free address
/// after the expr manifest; the return is the new high-water mark for
/// `reserve_memory`.
pub(crate) fn emit(
    splice: &mut Splice<'_>,
    program: &Program,
    layout: &crate::codegen::Layout,
    indices: &[u32],
    plan: &PackPlan,
    end: u32,
) -> u32 {
    let align8 = |v: u32| (v + 7) & !7;

    // --- Placement -------------------------------------------------------
    let manifest_at = align8(end);
    splice.data(manifest_at, plan.manifest.clone());
    let scratch = align8(manifest_at + plan.manifest.len() as u32);
    let mut cursor = scratch + 8;
    let entries: Vec<EntryLayout> = plan
        .entries
        .iter()
        .map(|entry| {
            let (func_at, func) = program
                .funcs
                .iter()
                .enumerate()
                .find(|(_, f)| f.system.is_some() && f.name == entry.name)
                .expect("every manifest system has a compiled func");
            let abi = func.system.as_ref().expect("filtered on system funcs");
            let block = cursor;
            cursor += GROUPS + entry.groups.len() as u32 * GROUP_STRIDE + 8;
            let rec = cursor;
            cursor += align8(8 + entry.frame_bytes);
            EntryLayout {
                block,
                rec,
                eval: indices[func_at],
                args: func.arg_buffers.iter().map(|b| layout.at(*b)).collect(),
                ret: layout.at(func.ret_buffer.expect("a system uses the buffer ABI")),
                rng: entry.rng_state.map(|i| layout.at(abi.state[i])),
                guard: abi.state.last().map(|b| layout.at(*b)),
            }
        })
        .collect();

    // --- Fixed-shape exports ---------------------------------------------
    let nullary = splice.ty(vec![], vec![ValType::I32]);
    let unary = splice.ty(vec![ValType::I32], vec![]);
    let describe_ty = splice.ty(vec![ValType::I32], vec![ValType::I64]);
    let accessor = splice.ty(vec![ValType::I32], vec![ValType::I32]);
    for (name, ty, constant) in [
        (
            "fsw_abi_version",
            nullary,
            Instruction::I32Const(plan.abi_version as i32),
        ),
        ("fsw_pack_open", nullary, Instruction::I32Const(1)),
        (
            "fsw_pack_describe",
            describe_ty,
            Instruction::I64Const(plan.manifest.len() as i64),
        ),
        (
            "fsw_pack_manifest_ptr",
            accessor,
            Instruction::I32Const(manifest_at as i32),
        ),
    ] {
        let mut body = Function::new([]);
        body.instruction(&constant);
        body.instruction(&Instruction::End);
        let index = splice.function(ty, body);
        splice.export(name, index);
    }
    for name in ["fsw_pack_shutdown", "fsw_pack_close"] {
        let mut body = Function::new([]);
        body.instruction(&Instruction::End);
        let index = splice.function(unary, body);
        splice.export(name, index);
    }

    // --- create / bind / execute / destroy --------------------------------
    let create_ty = splice.ty(vec![ValType::I32; 5], vec![ValType::I32]);
    let create = create(plan, &entries);
    let index = splice.function(create_ty, create);
    splice.export("fsw_pack_create", index);

    let bind_ty = splice.ty(vec![ValType::I32; 7], vec![]);
    let bind = bind_init(splice, plan, &entries);
    let index = splice.function(bind_ty, bind);
    splice.export("fsw_pack_bind_init", index);

    let execute_ty = splice.ty(vec![ValType::I32, ValType::I64], vec![ValType::I32]);
    let execute = execute(splice, plan, &entries, scratch);
    let index = splice.function(execute_ty, execute);
    splice.export("fsw_pack_execute", index);

    let destroy = destroy(splice, plan, &entries);
    let index = splice.function(unary, destroy);
    splice.export("fsw_pack_destroy", index);

    cursor
}

/// A zero-offset [`MemArg`] with an explicit alignment hint.
fn at(align: u32) -> MemArg {
    MemArg {
        offset: 0,
        align,
        memory_index: 0,
    }
}

/// A [`MemArg`] carrying a static offset, for loads off the record pointer.
fn off(offset: u32, align: u32) -> MemArg {
    MemArg {
        offset: offset as u64,
        align,
        memory_index: 0,
    }
}

/// `i32.store` of a constant at a constant address.
fn store_i32(body: &mut Function, addr: u32, value: i32) {
    body.instruction(&Instruction::I32Const(addr as i32));
    body.instruction(&Instruction::I32Const(value));
    body.instruction(&Instruction::I32Store(at(2)));
}

/// `i64.store` of a constant at a constant address.
fn store_i64(body: &mut Function, addr: u32, value: i64) {
    body.instruction(&Instruction::I32Const(addr as i32));
    body.instruction(&Instruction::I64Const(value));
    body.instruction(&Instruction::I64Store(at(3)));
}

/// `fsw_pack_create(pack, index, mount, params, params_len)`: dispatch on
/// the entry index, refuse a live double-create, zero the block, reset the
/// seed guard, store the host-injected `@rng` seed, and answer the block
/// address as the instance pointer.
fn create(plan: &PackPlan, entries: &[EntryLayout]) -> Function {
    let mut body = Function::new([]);
    for (i, (entry, lay)) in plan.entries.iter().zip(entries).enumerate() {
        let n = entry.groups.len() as u32;
        body.instruction(&Instruction::LocalGet(1));
        body.instruction(&Instruction::I32Const(i as i32));
        body.instruction(&Instruction::I32Eq);
        body.instruction(&Instruction::If(BlockType::Empty));
        {
            // A live instance already claims the static buffers.
            body.instruction(&Instruction::I32Const((lay.block + CREATED) as i32));
            body.instruction(&Instruction::I32Load(at(2)));
            body.instruction(&Instruction::If(BlockType::Empty));
            body.instruction(&Instruction::I32Const(0));
            body.instruction(&Instruction::Return);
            body.instruction(&Instruction::End);

            store_i32(&mut body, lay.block + CREATED, 1);
            store_i32(&mut body, lay.block + BOUND, 0);
            store_i64(&mut body, lay.block + COUNTDOWN, 0);
            store_i64(&mut body, lay.block + SEEN, 0);
            for g in 0..n {
                store_i32(&mut body, lay.view(g), 0);
                store_i64(&mut body, lay.last(g), 0);
            }
            store_i32(&mut body, lay.writer(n), 0);
            if let Some(guard) = lay.guard {
                // First eval re-seeds the annotated defaults.
                store_i32(&mut body, guard, 0);
            }
            if let Some(rng) = lay.rng {
                // The seed rides the params channel (host entropy); absent
                // params leave the zero seed, which is legal but shared.
                body.instruction(&Instruction::LocalGet(4));
                body.instruction(&Instruction::I32Const(8));
                body.instruction(&Instruction::I32GeS);
                body.instruction(&Instruction::If(BlockType::Empty));
                body.instruction(&Instruction::I32Const(rng as i32));
                body.instruction(&Instruction::LocalGet(3));
                body.instruction(&Instruction::I64Load(at(0)));
                body.instruction(&Instruction::I64Store(at(3)));
                body.instruction(&Instruction::End);
            }
            body.instruction(&Instruction::I32Const(lay.block as i32));
            body.instruction(&Instruction::Return);
        }
        body.instruction(&Instruction::End);
    }
    body.instruction(&Instruction::I32Const(0));
    body.instruction(&Instruction::End);
    body
}

/// `fsw_pack_bind_init(state, inputs, n_in, outputs, n_out, name, name_len)`:
/// open a view over each declared input's region and the writer over the
/// first output's, in the positional order the host stages the `FswRing`
/// arrays. Extra rings — the log port, a slot mount's tail — are simply
/// never opened; the host writes the entry's `system_status` itself.
/// Any failure leaves the instance unbound, which `execute` reports as
/// `Panicked`, matching the native ABI's caught-panic shape.
fn bind_init(splice: &Splice<'_>, plan: &PackPlan, entries: &[EntryLayout]) -> Function {
    let view_open = splice.kernel("pk_view_open");
    let writer_open = splice.kernel("pk_writer_open");
    let handle = 7; // one scratch local past the seven params
    let mut body = Function::new([(1, ValType::I32)]);
    for (entry, lay) in plan.entries.iter().zip(entries) {
        let n = entry.groups.len() as u32;
        body.instruction(&Instruction::LocalGet(0));
        body.instruction(&Instruction::I32Const(lay.block as i32));
        body.instruction(&Instruction::I32Eq);
        body.instruction(&Instruction::If(BlockType::Empty));
        {
            body.instruction(&Instruction::LocalGet(2));
            body.instruction(&Instruction::I32Const(n as i32));
            body.instruction(&Instruction::I32LtS);
            body.instruction(&Instruction::If(BlockType::Empty));
            body.instruction(&Instruction::Return);
            body.instruction(&Instruction::End);
            body.instruction(&Instruction::LocalGet(4));
            body.instruction(&Instruction::I32Eqz);
            body.instruction(&Instruction::If(BlockType::Empty));
            body.instruction(&Instruction::Return);
            body.instruction(&Instruction::End);

            // The host stages `FswRing` as wasm32 `{u32 base, u32 len, u8
            // role}` at a 12-byte stride.
            for g in 0..n {
                body.instruction(&Instruction::LocalGet(1));
                body.instruction(&Instruction::I32Load(off(g * 12, 2)));
                body.instruction(&Instruction::LocalGet(1));
                body.instruction(&Instruction::I32Load(off(g * 12 + 4, 2)));
                body.instruction(&Instruction::Call(view_open));
                body.instruction(&Instruction::LocalTee(handle));
                body.instruction(&Instruction::I32Eqz);
                body.instruction(&Instruction::If(BlockType::Empty));
                body.instruction(&Instruction::Return);
                body.instruction(&Instruction::End);
                body.instruction(&Instruction::I32Const(lay.view(g) as i32));
                body.instruction(&Instruction::LocalGet(handle));
                body.instruction(&Instruction::I32Store(at(2)));
            }
            body.instruction(&Instruction::LocalGet(3));
            body.instruction(&Instruction::I32Load(off(0, 2)));
            body.instruction(&Instruction::LocalGet(3));
            body.instruction(&Instruction::I32Load(off(4, 2)));
            body.instruction(&Instruction::Call(writer_open));
            body.instruction(&Instruction::LocalTee(handle));
            body.instruction(&Instruction::I32Eqz);
            body.instruction(&Instruction::If(BlockType::Empty));
            body.instruction(&Instruction::Return);
            body.instruction(&Instruction::End);
            body.instruction(&Instruction::I32Const(lay.writer(n) as i32));
            body.instruction(&Instruction::LocalGet(handle));
            body.instruction(&Instruction::I32Store(at(2)));

            store_i32(&mut body, lay.block + BOUND, 1);
            body.instruction(&Instruction::Return);
        }
        body.instruction(&Instruction::End);
    }
    body.instruction(&Instruction::End);
    body
}

/// `fsw_pack_execute(state, now)`: refresh every group whose ring moved,
/// apply the run rule, evaluate, publish. Statuses are the pack ABI's:
/// `0` running (a skipped cycle is still running), `1` panicked (unbound
/// instance, corrupt ring).
fn execute(
    splice: &Splice<'_>,
    plan: &PackPlan,
    entries: &[EntryLayout],
    scratch: u32,
) -> Function {
    let committed = splice.kernel("pk_view_committed");
    let latest = splice.kernel("pk_view_latest");
    let write = splice.kernel("pk_write");
    // Locals past the two params.
    let fired = 2i32 as u32;
    let rp = 3;
    let rl = 4;
    let got = 5;
    let c = 6; // i64
    let mut body = Function::new([(4, ValType::I32), (1, ValType::I64)]);
    for (entry, lay) in plan.entries.iter().zip(entries) {
        let n = entry.groups.len() as u32;
        body.instruction(&Instruction::LocalGet(0));
        body.instruction(&Instruction::I32Const(lay.block as i32));
        body.instruction(&Instruction::I32Eq);
        body.instruction(&Instruction::If(BlockType::Empty));
        {
            body.instruction(&Instruction::I32Const((lay.block + BOUND) as i32));
            body.instruction(&Instruction::I32Load(at(2)));
            body.instruction(&Instruction::I32Eqz);
            body.instruction(&Instruction::If(BlockType::Empty));
            body.instruction(&Instruction::I32Const(1));
            body.instruction(&Instruction::Return);
            body.instruction(&Instruction::End);

            body.instruction(&Instruction::I32Const(0));
            body.instruction(&Instruction::LocalSet(fired));

            for (g, group) in entry.groups.iter().enumerate() {
                let g = g as u32;
                // c = committed(view); if c != held, take the newest record.
                body.instruction(&Instruction::I32Const(lay.view(g) as i32));
                body.instruction(&Instruction::I32Load(at(2)));
                body.instruction(&Instruction::Call(committed));
                body.instruction(&Instruction::LocalTee(c));
                body.instruction(&Instruction::I32Const(lay.last(g) as i32));
                body.instruction(&Instruction::I64Load(at(3)));
                body.instruction(&Instruction::I64Ne);
                body.instruction(&Instruction::If(BlockType::Empty));
                {
                    body.instruction(&Instruction::I32Const(lay.view(g) as i32));
                    body.instruction(&Instruction::I32Load(at(2)));
                    body.instruction(&Instruction::I32Const(scratch as i32));
                    body.instruction(&Instruction::Call(latest));
                    body.instruction(&Instruction::LocalTee(got));
                    body.instruction(&Instruction::I32Const(0));
                    body.instruction(&Instruction::I32LtS);
                    body.instruction(&Instruction::If(BlockType::Empty));
                    body.instruction(&Instruction::I32Const(1));
                    body.instruction(&Instruction::Return);
                    body.instruction(&Instruction::End);
                    body.instruction(&Instruction::LocalGet(got));
                    body.instruction(&Instruction::If(BlockType::Empty));
                    {
                        body.instruction(&Instruction::I32Const(scratch as i32));
                        body.instruction(&Instruction::I32Load(at(2)));
                        body.instruction(&Instruction::LocalSet(rp));
                        body.instruction(&Instruction::I32Const((scratch + 4) as i32));
                        body.instruction(&Instruction::I32Load(at(2)));
                        body.instruction(&Instruction::LocalSet(rl));
                        // One validation gate on the foreign record: long
                        // enough to cover every fill, else not a sample.
                        body.instruction(&Instruction::LocalGet(rl));
                        body.instruction(&Instruction::I32Const(group.min_len as i32));
                        body.instruction(&Instruction::I32GeU);
                        body.instruction(&Instruction::If(BlockType::Empty));
                        {
                            for fill in &group.fills {
                                unpack(&mut body, lay, fill, rp);
                            }
                            // seen |= 1 << g; held = c.
                            body.instruction(&Instruction::I32Const((lay.block + SEEN) as i32));
                            body.instruction(&Instruction::I32Const((lay.block + SEEN) as i32));
                            body.instruction(&Instruction::I64Load(at(3)));
                            body.instruction(&Instruction::I64Const(1 << g));
                            body.instruction(&Instruction::I64Or);
                            body.instruction(&Instruction::I64Store(at(3)));
                            body.instruction(&Instruction::I32Const(lay.last(g) as i32));
                            body.instruction(&Instruction::LocalGet(c));
                            body.instruction(&Instruction::I64Store(at(3)));
                            if group.driving {
                                body.instruction(&Instruction::I32Const(1));
                                body.instruction(&Instruction::LocalSet(fired));
                            }
                        }
                        body.instruction(&Instruction::End);
                    }
                    body.instruction(&Instruction::End);
                }
                body.instruction(&Instruction::End);
            }

            match entry.rate_reload {
                // The decimated cycle clock: fire at zero, reload.
                Some(reload) => {
                    body.instruction(&Instruction::I32Const((lay.block + COUNTDOWN) as i32));
                    body.instruction(&Instruction::I64Load(at(3)));
                    body.instruction(&Instruction::I64Const(0));
                    body.instruction(&Instruction::I64Ne);
                    body.instruction(&Instruction::If(BlockType::Empty));
                    body.instruction(&Instruction::I32Const((lay.block + COUNTDOWN) as i32));
                    body.instruction(&Instruction::I32Const((lay.block + COUNTDOWN) as i32));
                    body.instruction(&Instruction::I64Load(at(3)));
                    body.instruction(&Instruction::I64Const(1));
                    body.instruction(&Instruction::I64Sub);
                    body.instruction(&Instruction::I64Store(at(3)));
                    body.instruction(&Instruction::I32Const(0));
                    body.instruction(&Instruction::Return);
                    body.instruction(&Instruction::End);
                    store_i64(&mut body, lay.block + COUNTDOWN, reload);
                }
                // Input-driven: skip the cycle unless the driving ring moved.
                None => {
                    body.instruction(&Instruction::LocalGet(fired));
                    body.instruction(&Instruction::I32Eqz);
                    body.instruction(&Instruction::If(BlockType::Empty));
                    body.instruction(&Instruction::I32Const(0));
                    body.instruction(&Instruction::Return);
                    body.instruction(&Instruction::End);
                }
            }

            // An input that has never published skips the cycle.
            if n > 0 {
                let full = if n == 63 { i64::MAX } else { (1i64 << n) - 1 };
                body.instruction(&Instruction::I32Const((lay.block + SEEN) as i32));
                body.instruction(&Instruction::I64Load(at(3)));
                body.instruction(&Instruction::I64Const(full));
                body.instruction(&Instruction::I64Ne);
                body.instruction(&Instruction::If(BlockType::Empty));
                body.instruction(&Instruction::I32Const(0));
                body.instruction(&Instruction::Return);
                body.instruction(&Instruction::End);
            }

            body.instruction(&Instruction::LocalGet(1));
            body.instruction(&Instruction::Call(lay.eval));
            body.instruction(&Instruction::Drop);

            // Publish: [now][frame], dropped silently when the ring is full.
            body.instruction(&Instruction::I32Const(lay.rec as i32));
            body.instruction(&Instruction::LocalGet(1));
            body.instruction(&Instruction::I64Store(at(3)));
            body.instruction(&Instruction::I32Const((lay.rec + 8) as i32));
            body.instruction(&Instruction::I32Const(lay.ret as i32));
            body.instruction(&Instruction::I32Const(entry.frame_bytes as i32));
            body.instruction(&Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });
            body.instruction(&Instruction::I32Const(lay.writer(n) as i32));
            body.instruction(&Instruction::I32Load(at(2)));
            body.instruction(&Instruction::I32Const(lay.rec as i32));
            body.instruction(&Instruction::I32Const((8 + entry.frame_bytes) as i32));
            body.instruction(&Instruction::Call(write));
            body.instruction(&Instruction::Drop);
            body.instruction(&Instruction::I32Const(0));
            body.instruction(&Instruction::Return);
        }
        body.instruction(&Instruction::End);
    }
    body.instruction(&Instruction::I32Const(1));
    body.instruction(&Instruction::End);
    body
}

/// `fsw_pack_destroy(state)`: close the ring handles (releasing their
/// region roles) and clear the block, so a fresh create is legal.
fn destroy(splice: &Splice<'_>, plan: &PackPlan, entries: &[EntryLayout]) -> Function {
    let view_close = splice.kernel("pk_view_close");
    let writer_close = splice.kernel("pk_writer_close");
    let mut body = Function::new([]);
    for (entry, lay) in plan.entries.iter().zip(entries) {
        let n = entry.groups.len() as u32;
        body.instruction(&Instruction::LocalGet(0));
        body.instruction(&Instruction::I32Const(lay.block as i32));
        body.instruction(&Instruction::I32Eq);
        body.instruction(&Instruction::If(BlockType::Empty));
        {
            for g in 0..n {
                body.instruction(&Instruction::I32Const(lay.view(g) as i32));
                body.instruction(&Instruction::I32Load(at(2)));
                body.instruction(&Instruction::Call(view_close));
            }
            body.instruction(&Instruction::I32Const(lay.writer(n) as i32));
            body.instruction(&Instruction::I32Load(at(2)));
            body.instruction(&Instruction::Call(writer_close));
            store_i32(&mut body, lay.block + CREATED, 0);
            store_i32(&mut body, lay.block + BOUND, 0);
            body.instruction(&Instruction::Return);
        }
        body.instruction(&Instruction::End);
    }
    body.instruction(&Instruction::End);
    body
}

/// One fill: copy `elements` values from the record at `rp` into the
/// argument frame, converting the producer's element type into the frame's
/// eight-byte slot. Addresses are constants; only `rp` is a runtime value.
fn unpack(body: &mut Function, lay: &EntryLayout, fill: &Fill, rp: u32) {
    let prim_size = fill.prim.size() as u32;
    for e in 0..fill.elements {
        let dst = lay.args[fill.param] + fill.slot_offset + e * 8;
        let src = fill.src_offset + e * prim_size;
        body.instruction(&Instruction::I32Const(dst as i32));
        body.instruction(&Instruction::LocalGet(rp));
        match (fill.slot, fill.prim) {
            (SlotTy::F64, PrimType::F64) => {
                body.instruction(&Instruction::F64Load(off(src, 0)));
            }
            (SlotTy::F64, PrimType::F32) => {
                body.instruction(&Instruction::F32Load(off(src, 0)));
                body.instruction(&Instruction::F64PromoteF32);
            }
            (SlotTy::F64, PrimType::I64) => {
                body.instruction(&Instruction::I64Load(off(src, 0)));
                body.instruction(&Instruction::F64ConvertI64S);
            }
            (SlotTy::F64, PrimType::U64) => {
                body.instruction(&Instruction::I64Load(off(src, 0)));
                body.instruction(&Instruction::F64ConvertI64U);
            }
            (SlotTy::F64, PrimType::I32) => {
                body.instruction(&Instruction::I32Load(off(src, 0)));
                body.instruction(&Instruction::F64ConvertI32S);
            }
            (SlotTy::F64, PrimType::U32) => {
                body.instruction(&Instruction::I32Load(off(src, 0)));
                body.instruction(&Instruction::F64ConvertI32U);
            }
            (SlotTy::F64, PrimType::I16) => {
                body.instruction(&Instruction::I32Load16S(off(src, 0)));
                body.instruction(&Instruction::F64ConvertI32S);
            }
            (SlotTy::F64, PrimType::U16) => {
                body.instruction(&Instruction::I32Load16U(off(src, 0)));
                body.instruction(&Instruction::F64ConvertI32U);
            }
            (SlotTy::F64, PrimType::I8) => {
                body.instruction(&Instruction::I32Load8S(off(src, 0)));
                body.instruction(&Instruction::F64ConvertI32S);
            }
            (SlotTy::F64, PrimType::U8 | PrimType::Bool) => {
                body.instruction(&Instruction::I32Load8U(off(src, 0)));
                body.instruction(&Instruction::F64ConvertI32U);
            }
            (SlotTy::Bool, _) => {
                body.instruction(&Instruction::I32Load8U(off(src, 0)));
            }
            (SlotTy::I64, _) => {
                body.instruction(&Instruction::I64Load(off(src, 0)));
            }
        }
        match fill.slot {
            SlotTy::F64 => body.instruction(&Instruction::F64Store(at(3))),
            SlotTy::I64 => body.instruction(&Instruction::I64Store(at(3))),
            SlotTy::Bool => body.instruction(&Instruction::I32Store(at(2))),
        };
    }
}
