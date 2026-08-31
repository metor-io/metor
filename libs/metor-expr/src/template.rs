//! Appending generated code to a prebuilt guest module.
//!
//! A compiled expression is the checked-in [`prelude.wasm`](crate) with new
//! functions on the end. Nothing links, nothing is relocated: the prelude has
//! no imports, so its function index space is exactly its defined functions,
//! and appending leaves every existing index — and therefore every existing
//! body — untouched.
//!
//! The one wrinkle is garbage collection. Shipping all sixty-odd prelude
//! functions in a module that only wanted `sin` is most of the module's bytes,
//! so unreachable functions are dropped, which renumbers the survivors. That
//! renumbering has to happen *before* codegen emits a single `call`, or
//! generated bodies would carry stale indices. Hence the two phases:
//!
//! - [`Template::plan`] takes the kernel names the type checker decided the
//!   program needs, walks the call graph from them, and returns a [`Plan`]
//!   holding final indices.
//! - [`Plan::splice`] then accepts generated functions and emits the module.
//!
//! The walk's roots are the required kernels plus anything the module reaches
//! without a static call — element-segment entries, `start`, and `ref.func` in
//! global initializers — because those are entered by index rather than by
//! name.

use std::collections::BTreeMap;
use std::convert::Infallible;

use wasm_encoder::reencode::Reencode;
use wasmparser::{Operator, Parser, Payload};

/// A prebuilt guest module that generated functions are appended to.
pub struct Template<'a> {
    types: Vec<wasmparser::RecGroup>,
    func_types: Vec<u32>,
    tables: Vec<wasmparser::Table<'a>>,
    memories: Vec<wasmparser::MemoryType>,
    globals: Vec<wasmparser::Global<'a>>,
    exports: Vec<(String, wasmparser::ExternalKind, u32)>,
    start: Option<u32>,
    elements: Vec<wasmparser::ElementItems<'a>>,
    element_offsets: Vec<wasmparser::ConstExpr<'a>>,
    bodies: Vec<wasmparser::FunctionBody<'a>>,
    data: Vec<(u32, &'a [u8])>,
    calls: Vec<Vec<u32>>,
    exported_funcs: BTreeMap<String, u32>,
}

/// Why a template could not be read, or a plan could not be made.
#[derive(Debug)]
pub enum TemplateError {
    /// The bytes are not a well-formed core module.
    Malformed(String),
    /// The template declares an import; compiled modules must stay closed.
    Imports,
    /// A required kernel is absent from the template's exports.
    MissingKernel(String),
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateError::Malformed(why) => write!(f, "malformed template module: {why}"),
            TemplateError::Imports => write!(f, "template module declares imports"),
            TemplateError::MissingKernel(name) => write!(f, "template exports no kernel `{name}`"),
        }
    }
}

impl std::error::Error for TemplateError {}

impl<'a> Template<'a> {
    /// Read a template out of a core module's bytes.
    pub fn parse(wasm: &'a [u8]) -> Result<Self, TemplateError> {
        let mut t = Template {
            types: Vec::new(),
            func_types: Vec::new(),
            tables: Vec::new(),
            memories: Vec::new(),
            globals: Vec::new(),
            exports: Vec::new(),
            start: None,
            elements: Vec::new(),
            element_offsets: Vec::new(),
            bodies: Vec::new(),
            data: Vec::new(),
            calls: Vec::new(),
            exported_funcs: BTreeMap::new(),
        };
        let bad = |e: wasmparser::BinaryReaderError| TemplateError::Malformed(e.to_string());

        for payload in Parser::new(0).parse_all(wasm) {
            match payload.map_err(bad)? {
                Payload::TypeSection(r) => {
                    for group in r {
                        t.types.push(group.map_err(bad)?);
                    }
                }
                Payload::ImportSection(r) if r.count() > 0 => return Err(TemplateError::Imports),
                Payload::FunctionSection(r) => {
                    for ty in r {
                        t.func_types.push(ty.map_err(bad)?);
                    }
                }
                Payload::TableSection(r) => {
                    for table in r {
                        t.tables.push(table.map_err(bad)?);
                    }
                }
                Payload::MemorySection(r) => {
                    for m in r {
                        t.memories.push(m.map_err(bad)?);
                    }
                }
                Payload::GlobalSection(r) => {
                    for g in r {
                        t.globals.push(g.map_err(bad)?);
                    }
                }
                Payload::ExportSection(r) => {
                    for e in r {
                        let e = e.map_err(bad)?;
                        if matches!(
                            e.kind,
                            wasmparser::ExternalKind::Func | wasmparser::ExternalKind::FuncExact
                        ) {
                            t.exported_funcs.insert(e.name.to_string(), e.index);
                        }
                        t.exports.push((e.name.to_string(), e.kind, e.index));
                    }
                }
                Payload::StartSection { func, .. } => t.start = Some(func),
                Payload::ElementSection(r) => {
                    for seg in r {
                        let seg = seg.map_err(bad)?;
                        let wasmparser::ElementKind::Active { offset_expr, .. } = seg.kind else {
                            return Err(TemplateError::Malformed("passive element segment".into()));
                        };
                        t.element_offsets.push(offset_expr);
                        t.elements.push(seg.items);
                    }
                }
                Payload::CodeSectionEntry(body) => {
                    t.calls.push(static_calls(&body).map_err(bad)?);
                    t.bodies.push(body);
                }
                Payload::DataSection(r) => {
                    for seg in r {
                        let seg = seg.map_err(bad)?;
                        let wasmparser::DataKind::Active { offset_expr, .. } = seg.kind else {
                            return Err(TemplateError::Malformed("passive data segment".into()));
                        };
                        let offset = const_i32(&offset_expr).map_err(bad)?;
                        t.data.push((offset as u32, seg.data));
                    }
                }
                _ => {}
            }
        }
        Ok(t)
    }

    /// Index of an exported function in the template's own numbering.
    pub fn export(&self, name: &str) -> Option<u32> {
        self.exported_funcs.get(name).copied()
    }

    /// First address in linear memory the linker left unclaimed.
    ///
    /// The guest has no allocator, so everything from here up belongs to the
    /// compiler: argument buffers, return buffers, temporaries, and the
    /// descriptors kernel call sites point at.
    pub fn heap_base(&self) -> u32 {
        let index = self
            .exports
            .iter()
            .find(|(name, kind, _)| {
                name == "__heap_base" && *kind == wasmparser::ExternalKind::Global
            })
            .map(|(_, _, index)| *index)
            .expect("wasm-ld always exports __heap_base");
        let init = &self.globals[index as usize].init_expr;
        const_i32(init).expect("__heap_base is an i32 constant") as u32
    }

    /// Number of functions the template defines.
    pub fn function_count(&self) -> u32 {
        self.func_types.len() as u32
    }

    /// Drop everything the named kernels cannot reach and fix the surviving
    /// indices, ready for codegen.
    pub fn plan(self, kernels: &[&str]) -> Result<Plan<'a>, TemplateError> {
        let mut roots = Vec::new();
        for name in kernels {
            let idx = self
                .export(name)
                .ok_or_else(|| TemplateError::MissingKernel((*name).to_string()))?;
            roots.push(idx);
        }
        for items in &self.elements {
            if let wasmparser::ElementItems::Functions(fs) = items {
                for f in fs.clone() {
                    roots.push(f.map_err(|e| TemplateError::Malformed(e.to_string()))?);
                }
            }
        }
        roots.extend(self.start);
        for g in &self.globals {
            if let Ok(Operator::RefFunc { function_index }) =
                g.init_expr.get_operators_reader().read()
            {
                roots.push(function_index);
            }
        }

        let total = self.func_types.len();
        let mut live = vec![false; total];
        let mut stack = roots;
        while let Some(f) = stack.pop() {
            let f = f as usize;
            if f >= total || live[f] {
                continue;
            }
            live[f] = true;
            stack.extend(self.calls[f].iter().copied());
        }

        let mut remap = vec![u32::MAX; total];
        let mut kept = 0u32;
        for (i, alive) in live.iter().enumerate() {
            if *alive {
                remap[i] = kept;
                kept += 1;
            }
        }

        let kernel_index = kernels
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    remap[self.export(name).unwrap() as usize],
                )
            })
            .collect();

        Ok(Plan {
            template: self,
            remap,
            kept,
            kernel_index,
        })
    }
}

/// A template whose surviving functions have their final indices.
pub struct Plan<'a> {
    template: Template<'a>,
    remap: Vec<u32>,
    kept: u32,
    kernel_index: BTreeMap<String, u32>,
}

impl<'a> Plan<'a> {
    /// Final index of a kernel named in [`Template::plan`].
    pub fn kernel(&self, name: &str) -> u32 {
        self.kernel_index[name]
    }

    /// Index the first generated function will take.
    pub fn first_generated(&self) -> u32 {
        self.kept
    }

    /// Functions the call-graph walk kept.
    pub fn kept(&self) -> u32 {
        self.kept
    }

    /// Begin appending generated code.
    pub fn splice(self) -> Splice<'a> {
        Splice {
            plan: self,
            types: Vec::new(),
            functions: Vec::new(),
            exports: Vec::new(),
            data: Vec::new(),
        }
    }
}

/// Generated types, functions, exports, and data waiting to be emitted.
pub struct Splice<'a> {
    plan: Plan<'a>,
    types: Vec<(Vec<wasm_encoder::ValType>, Vec<wasm_encoder::ValType>)>,
    functions: Vec<(u32, wasm_encoder::Function)>,
    exports: Vec<(String, u32)>,
    data: Vec<(u32, Vec<u8>)>,
}

impl Splice<'_> {
    /// Final index of a kernel named in [`Template::plan`].
    pub fn kernel(&self, name: &str) -> u32 {
        self.plan.kernel(name)
    }

    /// Declare a generated function type, returning its type index.
    pub fn ty(
        &mut self,
        params: Vec<wasm_encoder::ValType>,
        results: Vec<wasm_encoder::ValType>,
    ) -> u32 {
        let index = self.plan.template.types.len() + self.types.len();
        self.types.push((params, results));
        index as u32
    }

    /// Append a generated function, returning its final index.
    pub fn function(&mut self, ty: u32, body: wasm_encoder::Function) -> u32 {
        let index = self.plan.kept + self.functions.len() as u32;
        self.functions.push((ty, body));
        index
    }

    /// Index the next call to [`Splice::function`] will return.
    pub fn next_function(&self) -> u32 {
        self.plan.kept + self.functions.len() as u32
    }

    /// Export a function under a name the host will call it by.
    pub fn export(&mut self, name: impl Into<String>, func: u32) {
        self.exports.push((name.into(), func));
    }

    /// Place initialized bytes at an absolute address in linear memory.
    pub fn data(&mut self, offset: u32, bytes: Vec<u8>) {
        self.data.push((offset, bytes));
    }

    /// Raise the memory minimum so `bytes` of linear memory exist.
    pub fn reserve_memory(&mut self, bytes: u32) {
        let pages = u64::from(bytes).div_ceil(64 * 1024);
        let memory = &mut self.plan.template.memories[0];
        memory.initial = memory.initial.max(pages);
    }

    /// First address the compiler may place a buffer at.
    pub fn heap_base(&self) -> u32 {
        self.plan.template.heap_base()
    }

    /// Emit the finished module.
    pub fn finish(self) -> Vec<u8> {
        let Splice {
            plan,
            types,
            functions,
            exports,
            data,
        } = self;
        let Plan {
            template, remap, ..
        } = plan;
        let mut map = Remap { remap };
        let mut module = wasm_encoder::Module::new();

        let mut type_section = wasm_encoder::TypeSection::new();
        for group in &template.types {
            map.parse_recursive_type_group(type_section.ty(), group.clone())
                .unwrap();
        }
        for (params, results) in &types {
            type_section
                .ty()
                .function(params.iter().copied(), results.iter().copied());
        }
        module.section(&type_section);

        let mut function_section = wasm_encoder::FunctionSection::new();
        for (i, ty) in template.func_types.iter().enumerate() {
            if map.remap[i] != u32::MAX {
                function_section.function(*ty);
            }
        }
        for (ty, _) in &functions {
            function_section.function(*ty);
        }
        module.section(&function_section);

        if !template.tables.is_empty() {
            let mut section = wasm_encoder::TableSection::new();
            for table in &template.tables {
                let ty = map.table_type(table.ty).unwrap();
                match &table.init {
                    wasmparser::TableInit::RefNull => {
                        section.table(ty);
                    }
                    wasmparser::TableInit::Expr(expr) => {
                        section.table_with_init(ty, &map.const_expr(expr.clone()).unwrap());
                    }
                }
            }
            module.section(&section);
        }

        let mut memory_section = wasm_encoder::MemorySection::new();
        for m in &template.memories {
            memory_section.memory(map.memory_type(*m).unwrap());
        }
        module.section(&memory_section);

        if !template.globals.is_empty() {
            let mut section = wasm_encoder::GlobalSection::new();
            for g in &template.globals {
                section.global(
                    map.global_type(g.ty).unwrap(),
                    &map.const_expr(g.init_expr.clone()).unwrap(),
                );
            }
            module.section(&section);
        }

        let mut export_section = wasm_encoder::ExportSection::new();
        for (name, kind, index) in &template.exports {
            let index = match kind {
                wasmparser::ExternalKind::Func | wasmparser::ExternalKind::FuncExact => {
                    let new = map.remap[*index as usize];
                    if new == u32::MAX {
                        continue;
                    }
                    new
                }
                _ => *index,
            };
            export_section.export(name, map.export_kind(*kind), index);
        }
        for (name, func) in &exports {
            export_section.export(name, wasm_encoder::ExportKind::Func, *func);
        }
        module.section(&export_section);

        if let Some(func) = template.start {
            module.section(&wasm_encoder::StartSection {
                function_index: map.remap[func as usize],
            });
        }

        if !template.elements.is_empty() {
            let mut section = wasm_encoder::ElementSection::new();
            for (items, offset) in template.elements.iter().zip(&template.element_offsets) {
                let wasmparser::ElementItems::Functions(fs) = items else {
                    unreachable!("element items are validated as function lists at parse")
                };
                let indices: Vec<u32> = fs
                    .clone()
                    .into_iter()
                    .map(|f| map.remap[f.unwrap() as usize])
                    .collect();
                section.active(
                    None,
                    &map.const_expr(offset.clone()).unwrap(),
                    wasm_encoder::Elements::Functions(indices.into()),
                );
            }
            module.section(&section);
        }

        let mut code_section = wasm_encoder::CodeSection::new();
        for (i, body) in template.bodies.iter().enumerate() {
            if map.remap[i] != u32::MAX {
                map.parse_function_body(&mut code_section, body.clone())
                    .unwrap();
            }
        }
        for (_, body) in &functions {
            code_section.function(body);
        }
        module.section(&code_section);

        let mut data_section = wasm_encoder::DataSection::new();
        for (offset, bytes) in &template.data {
            data_section.active(
                0,
                &wasm_encoder::ConstExpr::i32_const(*offset as i32),
                bytes.iter().copied(),
            );
        }
        for (offset, bytes) in &data {
            data_section.active(
                0,
                &wasm_encoder::ConstExpr::i32_const(*offset as i32),
                bytes.iter().copied(),
            );
        }
        module.section(&data_section);

        module.finish()
    }
}

struct Remap {
    remap: Vec<u32>,
}

impl Reencode for Remap {
    type Error = Infallible;

    fn function_index(&mut self, func: u32) -> Result<u32, wasm_encoder::reencode::Error> {
        Ok(self.remap[func as usize])
    }
}

fn static_calls(body: &wasmparser::FunctionBody<'_>) -> wasmparser::Result<Vec<u32>> {
    let mut calls = Vec::new();
    let mut reader = body.get_operators_reader()?;
    while !reader.eof() {
        match reader.read()? {
            Operator::Call { function_index } | Operator::RefFunc { function_index } => {
                calls.push(function_index)
            }
            Operator::ReturnCall { function_index } => calls.push(function_index),
            _ => {}
        }
    }
    Ok(calls)
}

fn const_i32(expr: &wasmparser::ConstExpr<'_>) -> wasmparser::Result<i32> {
    match expr.get_operators_reader().read()? {
        Operator::I32Const { value } => Ok(value),
        _ => Ok(0),
    }
}

impl Remap {
    fn export_kind(&self, kind: wasmparser::ExternalKind) -> wasm_encoder::ExportKind {
        match kind {
            wasmparser::ExternalKind::Func | wasmparser::ExternalKind::FuncExact => {
                wasm_encoder::ExportKind::Func
            }
            wasmparser::ExternalKind::Table => wasm_encoder::ExportKind::Table,
            wasmparser::ExternalKind::Memory => wasm_encoder::ExportKind::Memory,
            wasmparser::ExternalKind::Global => wasm_encoder::ExportKind::Global,
            wasmparser::ExternalKind::Tag => wasm_encoder::ExportKind::Tag,
        }
    }
}
