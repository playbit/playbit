//! Portable scalar atomics for the runtime's single, private wasm32 memory.
//!
//! Bundle verification continues to use the original bytes. Only Pulley's
//! compilation input is rewritten; no imports, indices or data are changed.
use std::borrow::Cow;
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{CodeSection, Function, Instruction as I, MemArg, Module, RawSection, ValType};
use wasmtime::wasmparser::{self as parser, Operator, Payload, TypeRef, WasmFeatures};

// Matches the runtime bundle payload cap; transformed code has the same cap.
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_LOCALS: u32 = 50_000; // wasmparser's function-local validation limit.
const ATOMIC_RESERVE: usize = 512; // Strict upper bound on one expansion below.

// Wasmtime uses Range<u64> which is annoying to deal with here, paper over that
trait NicerRange {
    fn len(&self) -> usize;
    fn usize_range(&self) -> std::ops::Range<usize>;
}

impl NicerRange for std::ops::Range<u64> {
    fn len(&self) -> usize {
        self.end as usize - self.start as usize
    }

    fn usize_range(&self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// Validate before rewriting so invalid atomic encodings never become valid.
/// Both backends enforce the same portable limits and validate the lowering.
/// Native then executes the original atomics; Pulley executes the lowered code.
/// `features` must be the engine's features, with only THREADS changed here.
pub(crate) fn prepare<'a>(
    wasm: &'a [u8],
    mut features: WasmFeatures,
    lower_for_pulley: bool,
) -> Result<Cow<'a, [u8]>, String> {
    if wasm.len() > MAX_BYTES {
        return Err("Guest Wasm exceeds the 128 MiB limit".into());
    }
    match parser::Parser::new(0).parse_all(wasm).next() {
        Some(Ok(Payload::Version { encoding: parser::Encoding::Module, .. })) => {}
        _ => return Err("Guest must be a core Wasm module".into()),
    }
    features.set(WasmFeatures::THREADS, true);
    let types = parser::Validator::new_with_features(features)
        .validate_all(wasm)
        .map_err(|e| e.to_string())?;
    let types = types.as_ref();
    validate_memories(types)?;
    let mut contains_atomics = false;
    for payload in parser::Parser::new(0).parse_all(wasm) {
        if let Payload::CodeSectionEntry(body) = payload.map_err(|e| e.to_string())? {
            for op in body.get_operators_reader().map_err(|e| e.to_string())? {
                if atomic(&op.map_err(|e| e.to_string())?).is_some() {
                    contains_atomics = true;
                    break;
                }
            }
        }
        if contains_atomics {
            break;
        }
    }
    if !contains_atomics {
        return Ok(Cow::Borrowed(wasm));
    }
    let bytes = lower_module(wasm, types)?;
    features.set(WasmFeatures::THREADS, false);
    parser::Validator::new_with_features(features)
        .validate_all(&bytes)
        .map_err(|e| format!("Invalid lowered atomics: {e}"))?;
    Ok(if lower_for_pulley { Cow::Owned(bytes) } else { Cow::Borrowed(wasm) })
}

fn validate_memories(types: parser::types::TypesRef<'_>) -> Result<(), String> {
    for index in 0..types.memory_count() {
        let memory = types.memory_at(index);
        if memory.shared {
            return Err("Shared guest memory is not supported".into());
        }
        if memory.memory64 {
            return Err("Guest memory must use wasm32 addressing".into());
        }
        if memory.page_size_log2() != 16 {
            return Err("Guest memory must use 64 KiB pages".into());
        }
    }
    if types.memory_count() > 1 {
        return Err("Guests may use only one memory".into());
    }
    assert!(types.memory_count() <= 1);
    Ok(())
}

fn lower_module(wasm: &[u8], types: parser::types::TypesRef<'_>) -> Result<Vec<u8>, String> {
    assert!(wasm.len() <= MAX_BYTES);
    assert!(types.memory_count() <= 1);
    let mut module = Module::new();
    let mut code: Option<CodeSection> = None;
    let mut function_index = 0;
    for payload in parser::Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| e.to_string())?;
        if !matches!(payload, Payload::CodeSectionEntry(_))
            && let Some(section) = code.take()
        {
            check_size(module.len(), section.byte_len() + 6)?;
            module.section(&section);
        }
        match payload {
            Payload::CodeSectionStart { .. } => code = Some(CodeSection::new()),
            Payload::CodeSectionEntry(body) => {
                let section = code.as_mut().ok_or("Missing code section")?;
                let id = types.core_function_at(function_index);
                let params =
                    types.get(id).ok_or("Missing function type")?.unwrap_func().params().len();
                function_index += 1;
                let mut has_atomic = false;
                for op in body.get_operators_reader().map_err(|e| e.to_string())? {
                    has_atomic |= atomic(&op.map_err(|e| e.to_string())?).is_some();
                }
                if !has_atomic {
                    check_size(module.len() + section.byte_len(), body.range().len() + 6)?;
                    section.raw(&wasm[body.range().usize_range()]);
                    continue;
                }
                let function =
                    lower_function(wasm, &body, params, module.len() + section.byte_len())?;
                section.function(&function);
            }
            other => {
                if let Payload::ImportSection(ref imports) = other {
                    for import in imports.clone().into_imports() {
                        if matches!(
                            import.map_err(|e| e.to_string())?.ty,
                            TypeRef::Func(_) | TypeRef::FuncExact(_)
                        ) {
                            function_index += 1;
                        }
                    }
                }
                if let Some((id, range)) = other.as_section() {
                    check_size(module.len(), range.len() + 6)?;
                    module.section(&RawSection { id, data: &wasm[range.usize_range()] });
                }
            }
        }
    }
    Ok(module.finish())
}

fn lower_function(
    wasm: &[u8],
    body: &parser::FunctionBody<'_>,
    params: usize,
    current: usize,
) -> Result<Function, String> {
    assert!(current <= MAX_BYTES);
    assert!(body.range().usize_range().end <= wasm.len());
    let mut reencoder = RoundtripReencoder;
    let mut locals = Vec::new();
    let mut local_count = 0u32;
    for local in body.get_locals_reader().map_err(|e| e.to_string())? {
        let (count, ty) = local.map_err(|e| e.to_string())?;
        local_count = local_count.checked_add(count).ok_or("Too many guest locals")?;
        locals.push((count, reencoder.val_type(ty).map_err(|e| e.to_string())?));
    }
    let scratch = u32::try_from(params)
        .map_err(|_| "Too many parameters")?
        .checked_add(local_count)
        .ok_or("Too many guest locals")?;
    // wasmparser counts parameters in the same local index space.
    if scratch > MAX_LOCALS - 7 {
        return Err("Atomic lowering exceeds the guest local limit".into());
    }
    locals.extend([(3, ValType::I32), (4, ValType::I64)]);
    let mut function = Function::new(locals);
    let mut ops = body.get_operators_reader().map_err(|e| e.to_string())?;
    while !ops.eof() {
        let start = ops.original_position() as usize;
        let op = ops.read().map_err(|e| e.to_string())?;
        if let Some(op) = atomic(&op) {
            check_size(current + function.byte_len(), ATOMIC_RESERVE + 12)?;
            let before = function.byte_len();
            emit(&mut function, scratch, op)?;
            if function.byte_len() - before > ATOMIC_RESERVE {
                return Err("Atomic expansion exceeded its bound".into());
            }
        } else {
            let bytes = &wasm[start..ops.original_position() as usize];
            check_size(current + function.byte_len(), bytes.len() + 12)?;
            function.raw(bytes.iter().copied());
        }
    }
    check_size(current, function.byte_len() + 12)?;
    assert!(function.byte_len() <= MAX_BYTES - current);
    Ok(function)
}

fn check_size(current: usize, additional: usize) -> Result<(), String> {
    if current.checked_add(additional).is_none_or(|size| size > MAX_BYTES) {
        Err("Atomic lowering exceeds the 128 MiB guest code limit".into())
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Load,
    Store,
    Add,
    Sub,
    And,
    Or,
    Xor,
    Exchange,
    CompareExchange,
    Notify,
    Wait,
    Fence,
}
#[derive(Clone, Copy)]
struct Atomic {
    kind: Kind,
    wide: bool,
    bytes: u8,
    memory: parser::MemArg,
}

fn emit(f: &mut Function, base: u32, op: Atomic) -> Result<(), String> {
    use Kind::*;
    if matches!(op.kind, Fence) {
        // A fence has no synchronization work on private memory, but must not
        // become free: Wasmtime charges const one fuel unit and drop zero.
        f.instruction(&I::I32Const(0));
        f.instruction(&I::Drop);
        return Ok(());
    }
    // Original module validation plus this guard make the i64 addition exact:
    // both wasm32 address and offset are <= u32::MAX, so they cannot wrap i64.
    if op.memory.memory != 0 || op.memory.offset > u32::MAX as u64 {
        return Err("Unsupported atomic memory address".into());
    }
    assert!(base <= MAX_LOCALS - 7);
    assert!(matches!(op.bytes, 1 | 2 | 4 | 8));
    emit_address_check(f, base, op);
    let ea = base + 3;
    let value = if op.wide { base + 4 } else { base };
    let expected = value + 1;
    let old = value + 2;
    assert!(old < base + 7);
    assert!(ea < base + 7);
    if matches!(op.kind, Wait) {
        f.instruction(&I::Unreachable); // Waiting on unshared memory always traps.
        return Ok(());
    }
    if matches!(op.kind, Notify) {
        f.instruction(&I::I32Const(0)); // This host never creates an unshared waiter.
        return Ok(());
    }
    let arg = MemArg { offset: 0, align: op.bytes.trailing_zeros(), memory_index: 0 };
    if matches!(op.kind, Store) {
        address(f, ea);
        f.instruction(&I::LocalGet(value));
        store(f, op, arg);
        return Ok(());
    }
    address(f, ea);
    load(f, op, arg);
    if matches!(op.kind, Load) {
        return Ok(());
    }
    f.instruction(&I::LocalSet(old));
    if matches!(op.kind, CompareExchange) {
        emit_compare_exchange(f, op, arg, ea, value, expected, old);
    } else {
        emit_update(f, op, arg, ea, value, old)?;
    }
    f.instruction(&I::LocalGet(old));
    Ok(())
}
fn emit_address_check(f: &mut Function, base: u32, op: Atomic) {
    use Kind::*;
    assert!(base <= MAX_LOCALS - 7);
    assert!(op.bytes.is_power_of_two());
    let ea = base + 3;
    let value = if op.wide { base + 4 } else { base };
    let expected = value + 1;
    match op.kind {
        Store | Add | Sub | And | Or | Xor | Exchange => {
            f.instruction(&I::LocalSet(value));
        }
        CompareExchange => {
            f.instruction(&I::LocalSet(value));
            f.instruction(&I::LocalSet(expected));
        }
        Notify => {
            f.instruction(&I::Drop);
        }
        Wait => {
            f.instruction(&I::Drop);
            f.instruction(&I::Drop);
        }
        Load | Fence => {}
    }
    f.instruction(&I::I64ExtendI32U);
    f.instruction(&I::I64Const(op.memory.offset as i64));
    f.instruction(&I::I64Add);
    f.instruction(&I::LocalSet(ea));
    // Atomic alignment is a runtime requirement, unlike ordinary loads/stores.
    f.instruction(&I::LocalGet(ea));
    f.instruction(&I::I64Const(i64::from(op.bytes - 1)));
    f.instruction(&I::I64And);
    f.instruction(&I::I64Eqz);
    f.instruction(&I::I32Eqz);
    trap_if(f);
    // Compare the last accessed byte's page, avoiding memory.size * 65536
    // overflow at the full 4 GiB wasm32 limit. Check before any mutation.
    f.instruction(&I::LocalGet(ea));
    f.instruction(&I::I64Const(i64::from(op.bytes - 1)));
    f.instruction(&I::I64Add);
    f.instruction(&I::I64Const(16));
    f.instruction(&I::I64ShrU);
    f.instruction(&I::MemorySize(0));
    f.instruction(&I::I64ExtendI32U);
    f.instruction(&I::I64GeU);
    trap_if(f);
    assert!(op.bytes <= if op.wide { 8 } else { 4 });
}

fn emit_compare_exchange(
    f: &mut Function,
    op: Atomic,
    arg: MemArg,
    ea: u32,
    value: u32,
    expected: u32,
    old: u32,
) {
    assert!(matches!(op.kind, Kind::CompareExchange));
    assert_eq!(expected, value + 1);
    assert_eq!(old, value + 2);
    f.instruction(&I::LocalGet(old));
    f.instruction(&I::LocalGet(expected));
    // Narrow CAS compares truncated operands, then returns zero-extended old.
    if op.bytes < if op.wide { 8 } else { 4 } {
        let mask = (1u64 << (op.bytes * 8)) - 1;
        f.instruction(&if op.wide { I::I64Const(mask as i64) } else { I::I32Const(mask as i32) });
        f.instruction(&if op.wide { I::I64And } else { I::I32And });
    }
    f.instruction(&if op.wide { I::I64Eq } else { I::I32Eq });
    f.instruction(&I::If(wasm_encoder::BlockType::Empty));
    address(f, ea);
    f.instruction(&I::LocalGet(value));
    store(f, op, arg);
    f.instruction(&I::End);
}

fn emit_update(
    f: &mut Function,
    op: Atomic,
    arg: MemArg,
    ea: u32,
    value: u32,
    old: u32,
) -> Result<(), String> {
    use Kind::*;
    assert_eq!(old, value + 2);
    assert!(op.bytes <= if op.wide { 8 } else { 4 });
    address(f, ea);
    if !matches!(op.kind, Exchange) {
        f.instruction(&I::LocalGet(old));
    }
    f.instruction(&I::LocalGet(value));
    let operation = match (op.kind, op.wide) {
        (Add, false) => Some(I::I32Add),
        (Add, true) => Some(I::I64Add),
        (Sub, false) => Some(I::I32Sub),
        (Sub, true) => Some(I::I64Sub),
        (And, false) => Some(I::I32And),
        (And, true) => Some(I::I64And),
        (Or, false) => Some(I::I32Or),
        (Or, true) => Some(I::I64Or),
        (Xor, false) => Some(I::I32Xor),
        (Xor, true) => Some(I::I64Xor),
        (Exchange, _) => None,
        _ => return Err("Invalid atomic operation".into()),
    };
    if let Some(operation) = operation {
        f.instruction(&operation);
    }
    store(f, op, arg);
    Ok(())
}

fn trap_if(f: &mut Function) {
    f.instruction(&I::If(wasm_encoder::BlockType::Empty));
    f.instruction(&I::Unreachable);
    f.instruction(&I::End);
}
fn address(f: &mut Function, ea: u32) {
    f.instruction(&I::LocalGet(ea));
    f.instruction(&I::I32WrapI64);
}
fn load(f: &mut Function, op: Atomic, arg: MemArg) {
    assert!(op.bytes.is_power_of_two());
    assert!(op.bytes <= if op.wide { 8 } else { 4 });
    assert_eq!(arg.memory_index, 0);
    assert_eq!(arg.offset, 0);
    assert_eq!(arg.align, op.bytes.trailing_zeros());
    f.instruction(&match (op.wide, op.bytes) {
        (false, 1) => I::I32Load8U(arg),
        (false, 2) => I::I32Load16U(arg),
        (false, 4) => I::I32Load(arg),
        (true, 1) => I::I64Load8U(arg),
        (true, 2) => I::I64Load16U(arg),
        (true, 4) => I::I64Load32U(arg),
        (true, 8) => I::I64Load(arg),
        _ => unreachable!(),
    });
}
fn store(f: &mut Function, op: Atomic, arg: MemArg) {
    assert!(op.bytes.is_power_of_two());
    assert!(op.bytes <= if op.wide { 8 } else { 4 });
    assert_eq!(arg.memory_index, 0);
    assert_eq!(arg.offset, 0);
    assert_eq!(arg.align, op.bytes.trailing_zeros());
    f.instruction(&match (op.wide, op.bytes) {
        (false, 1) => I::I32Store8(arg),
        (false, 2) => I::I32Store16(arg),
        (false, 4) => I::I32Store(arg),
        (true, 1) => I::I64Store8(arg),
        (true, 2) => I::I64Store16(arg),
        (true, 4) => I::I64Store32(arg),
        (true, 8) => I::I64Store(arg),
        _ => unreachable!(),
    });
}

fn atomic(op: &Operator<'_>) -> Option<Atomic> {
    use Kind::*;
    let (kind, wide, bytes, memory) = match *op {
        Operator::I32AtomicLoad { memarg } => (Load, false, 4, memarg),
        Operator::I32AtomicStore { memarg } => (Store, false, 4, memarg),
        Operator::I32AtomicRmwAdd { memarg } => (Add, false, 4, memarg),
        Operator::I32AtomicRmwSub { memarg } => (Sub, false, 4, memarg),
        Operator::I32AtomicRmwAnd { memarg } => (And, false, 4, memarg),
        Operator::I32AtomicRmwOr { memarg } => (Or, false, 4, memarg),
        Operator::I32AtomicRmwXor { memarg } => (Xor, false, 4, memarg),
        Operator::I32AtomicRmwXchg { memarg } => (Exchange, false, 4, memarg),
        Operator::I32AtomicRmwCmpxchg { memarg } => (CompareExchange, false, 4, memarg),
        Operator::I32AtomicLoad8U { memarg } => (Load, false, 1, memarg),
        Operator::I32AtomicStore8 { memarg } => (Store, false, 1, memarg),
        Operator::I32AtomicRmw8AddU { memarg } => (Add, false, 1, memarg),
        Operator::I32AtomicRmw8SubU { memarg } => (Sub, false, 1, memarg),
        Operator::I32AtomicRmw8AndU { memarg } => (And, false, 1, memarg),
        Operator::I32AtomicRmw8OrU { memarg } => (Or, false, 1, memarg),
        Operator::I32AtomicRmw8XorU { memarg } => (Xor, false, 1, memarg),
        Operator::I32AtomicRmw8XchgU { memarg } => (Exchange, false, 1, memarg),
        Operator::I32AtomicRmw8CmpxchgU { memarg } => (CompareExchange, false, 1, memarg),
        Operator::I32AtomicLoad16U { memarg } => (Load, false, 2, memarg),
        Operator::I32AtomicStore16 { memarg } => (Store, false, 2, memarg),
        Operator::I32AtomicRmw16AddU { memarg } => (Add, false, 2, memarg),
        Operator::I32AtomicRmw16SubU { memarg } => (Sub, false, 2, memarg),
        Operator::I32AtomicRmw16AndU { memarg } => (And, false, 2, memarg),
        Operator::I32AtomicRmw16OrU { memarg } => (Or, false, 2, memarg),
        Operator::I32AtomicRmw16XorU { memarg } => (Xor, false, 2, memarg),
        Operator::I32AtomicRmw16XchgU { memarg } => (Exchange, false, 2, memarg),
        Operator::I32AtomicRmw16CmpxchgU { memarg } => (CompareExchange, false, 2, memarg),
        Operator::MemoryAtomicNotify { memarg } => (Notify, false, 4, memarg),
        Operator::MemoryAtomicWait32 { memarg } => (Wait, false, 4, memarg),
        Operator::MemoryAtomicWait64 { memarg } => (Wait, true, 8, memarg),
        Operator::AtomicFence => {
            return Some(Atomic {
                kind: Fence,
                wide: false,
                bytes: 0,
                memory: parser::MemArg { align: 0, max_align: 0, offset: 0, memory: 0 },
            });
        }
        _ => return atomic_i64(op),
    };
    Some(Atomic { kind, wide, bytes, memory })
}

fn atomic_i64(op: &Operator<'_>) -> Option<Atomic> {
    use Kind::*;
    let (kind, wide, bytes, memory) = match *op {
        Operator::I64AtomicLoad { memarg } => (Load, true, 8, memarg),
        Operator::I64AtomicStore { memarg } => (Store, true, 8, memarg),
        Operator::I64AtomicRmwAdd { memarg } => (Add, true, 8, memarg),
        Operator::I64AtomicRmwSub { memarg } => (Sub, true, 8, memarg),
        Operator::I64AtomicRmwAnd { memarg } => (And, true, 8, memarg),
        Operator::I64AtomicRmwOr { memarg } => (Or, true, 8, memarg),
        Operator::I64AtomicRmwXor { memarg } => (Xor, true, 8, memarg),
        Operator::I64AtomicRmwXchg { memarg } => (Exchange, true, 8, memarg),
        Operator::I64AtomicRmwCmpxchg { memarg } => (CompareExchange, true, 8, memarg),
        Operator::I64AtomicLoad8U { memarg } => (Load, true, 1, memarg),
        Operator::I64AtomicStore8 { memarg } => (Store, true, 1, memarg),
        Operator::I64AtomicRmw8AddU { memarg } => (Add, true, 1, memarg),
        Operator::I64AtomicRmw8SubU { memarg } => (Sub, true, 1, memarg),
        Operator::I64AtomicRmw8AndU { memarg } => (And, true, 1, memarg),
        Operator::I64AtomicRmw8OrU { memarg } => (Or, true, 1, memarg),
        Operator::I64AtomicRmw8XorU { memarg } => (Xor, true, 1, memarg),
        Operator::I64AtomicRmw8XchgU { memarg } => (Exchange, true, 1, memarg),
        Operator::I64AtomicRmw8CmpxchgU { memarg } => (CompareExchange, true, 1, memarg),
        Operator::I64AtomicLoad16U { memarg } => (Load, true, 2, memarg),
        Operator::I64AtomicStore16 { memarg } => (Store, true, 2, memarg),
        Operator::I64AtomicRmw16AddU { memarg } => (Add, true, 2, memarg),
        Operator::I64AtomicRmw16SubU { memarg } => (Sub, true, 2, memarg),
        Operator::I64AtomicRmw16AndU { memarg } => (And, true, 2, memarg),
        Operator::I64AtomicRmw16OrU { memarg } => (Or, true, 2, memarg),
        Operator::I64AtomicRmw16XorU { memarg } => (Xor, true, 2, memarg),
        Operator::I64AtomicRmw16XchgU { memarg } => (Exchange, true, 2, memarg),
        Operator::I64AtomicRmw16CmpxchgU { memarg } => (CompareExchange, true, 2, memarg),
        Operator::I64AtomicLoad32U { memarg } => (Load, true, 4, memarg),
        Operator::I64AtomicStore32 { memarg } => (Store, true, 4, memarg),
        Operator::I64AtomicRmw32AddU { memarg } => (Add, true, 4, memarg),
        Operator::I64AtomicRmw32SubU { memarg } => (Sub, true, 4, memarg),
        Operator::I64AtomicRmw32AndU { memarg } => (And, true, 4, memarg),
        Operator::I64AtomicRmw32OrU { memarg } => (Or, true, 4, memarg),
        Operator::I64AtomicRmw32XorU { memarg } => (Xor, true, 4, memarg),
        Operator::I64AtomicRmw32XchgU { memarg } => (Exchange, true, 4, memarg),
        Operator::I64AtomicRmw32CmpxchgU { memarg } => (CompareExchange, true, 4, memarg),
        _ => return None,
    };
    assert!(wide);
    assert!(matches!(bytes, 1 | 2 | 4 | 8));
    Some(Atomic { kind, wide, bytes, memory })
}

#[cfg(test)]
mod tests;
