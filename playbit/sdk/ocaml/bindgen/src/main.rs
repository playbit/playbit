//! Generate the supported scalar ABI directly from the current Rust wrappers.
//! The small operation list describes semantics Rust signatures cannot express.
use std::{collections::BTreeMap, error::Error, fmt::Write as _, fs, path::Path};
use syn::{Expr, FnArg, Item, Pat, Stmt, Type, visit::Visit};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

// Borrowed buffers last for one call. The handwritten OCaml adapter owns retries,
// string copies and error interpretation; this generator owns scalar lowering.
const OPERATIONS: &[(&str, &[(&str, &str)])] = &[
    ("pb_log", &[("text", "*const u8"), ("length", "u32")]),
    ("pb_view_submit", &[("scene", "*const u8"), ("length", "u32")]),
    ("pb_event_poll", &[("event", "*mut Event"), ("data", "*mut u8"), ("capacity", "u32")]),
    (
        "pb_event_poll_deadline",
        &[
            ("event", "*mut Event"),
            ("data", "*mut u8"),
            ("capacity", "u32"),
            ("deadline_ns", "u64"),
        ],
    ),
    ("pb_host_limit", &[("limit", "u32")]),
    ("pb_host_platform", &[]),
    ("pb_quit_on_last_window", &[("intent", "u32")]),
];

#[derive(Default)]
struct Invocation<'a> {
    calls: Vec<&'a syn::ExprCall>,
}
impl<'ast> Visit<'ast> for Invocation<'ast> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = &*call.func
            && path.path.segments.last().is_some_and(|s| s.ident == "invoke")
        {
            self.calls.push(call);
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn type_name(ty: &Type) -> Result<String> {
    match ty {
        Type::Path(p) if p.path.segments.len() == 1 => Ok(p.path.segments[0].ident.to_string()),
        Type::Ptr(p) => Ok(format!(
            "*{} {}",
            if p.mutability.is_some() { "mut" } else { "const" },
            type_name(&p.elem)?
        )),
        _ => Err(format!("unsupported parameter type: {ty:?}").into()),
    }
}

fn integer(expr: &Expr, constants: &BTreeMap<String, i64>) -> Result<i64> {
    match expr {
        Expr::Lit(l) => {
            if let syn::Lit::Int(i) = &l.lit {
                Ok(i.base10_parse()?)
            } else {
                Err("expected integer".into())
            }
        }
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => Ok(-integer(&u.expr, constants)?),
        Expr::Path(p) => constants
            .get(&p.path.segments.last().ok_or("empty path")?.ident.to_string())
            .copied()
            .ok_or_else(|| "unknown constant".into()),
        Expr::Paren(p) => integer(&p.expr, constants),
        Expr::Field(f) if matches!(&f.member, syn::Member::Unnamed(i) if i.index == 0) => {
            integer(&f.base, constants)
        }
        Expr::Binary(b) => {
            let a = integer(&b.left, constants)?;
            let c = integer(&b.right, constants)?;
            match b.op {
                syn::BinOp::Mul(_) => Ok(a * c),
                syn::BinOp::Add(_) => Ok(a + c),
                syn::BinOp::Sub(_) => Ok(a - c),
                syn::BinOp::Shl(_) => Ok(a << c),
                syn::BinOp::BitOr(_) => Ok(a | c),
                _ => Err("unsupported constant expression".into()),
            }
        }
        _ => Err(format!("unsupported constant: {expr:?}").into()),
    }
}

fn enums(
    file: &syn::File,
    names: &[&str],
    ml: &mut String,
    values: &mut BTreeMap<String, i64>,
) -> Result<()> {
    for item in &file.items {
        if let Item::Enum(e) = item {
            let name = e.ident.to_string();
            if !names.contains(&name.as_str()) {
                continue;
            }
            let mut next = 0;
            writeln!(ml, "module {name} = struct")?;
            for v in &e.variants {
                if !matches!(v.fields, syn::Fields::Unit) {
                    return Err("enum payload unsupported".into());
                }
                let n = match &v.discriminant {
                    Some((_, e)) => integer(e, values)?,
                    None => next,
                };
                values.insert(format!("{name}::{}", v.ident), n);
                let mut field = snake(&v.ident.to_string());
                if name == "Key" && field == "end" {
                    field.push('_');
                }
                if name == "EventKind" && n & (1 << 31) != 0 {
                    // Advisory event kinds exceed a wasm OCaml int's 31-bit range.
                    writeln!(ml, "    let {field} = 0x{n:08x}l")?;
                } else {
                    writeln!(ml, "    let {field} = {n}")?;
                }
                next = n + 1;
            }
            if name == "ThemeColor" || name == "Key" {
                writeln!(ml, "    let count = {next}")?;
            }
            writeln!(ml, "end\n")?;
        }
    }
    Ok(())
}

fn snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_')
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

// Preserve the Rust module's flag names and its composite STANDARD value.
fn window_style_flags(
    ui: &syn::File,
    ml: &mut String,
    values: &mut BTreeMap<String, i64>,
) -> Result<()> {
    writeln!(ml, "module WindowStyle = struct")?;
    let mut mask = 0;
    let mut flags = BTreeMap::new();
    for item in &ui.items {
        let Item::Impl(block) = item else { continue };
        if !matches!(&*block.self_ty, Type::Path(p) if p.path.is_ident("WindowStyle")) {
            continue;
        }
        for item in &block.items {
            let syn::ImplItem::Const(flag) = item else { continue };
            let Expr::Call(call) = &flag.expr else {
                return Err("expected WindowStyle value".into());
            };
            if !matches!(&*call.func, Expr::Path(p) if p.path.is_ident("Self"))
                || call.args.len() != 1
            {
                return Err("unsupported WindowStyle constructor".into());
            }
            let bits = integer(&call.args[0], &flags)?;
            mask |= bits;
            flags.insert(flag.ident.to_string(), bits);
            values.insert(format!("WindowStyle::{}", flag.ident), bits);
            writeln!(ml, "    let {} = {bits}", flag.ident.to_string().to_lowercase())?;
        }
    }
    if flags.is_empty() {
        return Err("missing WindowStyle flags".into());
    }
    writeln!(ml, "    let mask = {mask}\nend\n")?;
    Ok(())
}

fn lower(expr: &Expr, params: &BTreeMap<String, String>) -> Result<String> {
    match expr {
        Expr::Lit(l) if matches!(&l.lit, syn::Lit::Int(i) if i.base10_digits()=="0") => {
            Ok("(i64.const 0)".into())
        }
        Expr::Path(p) => {
            let name = p.path.get_ident().ok_or("qualified parameter")?.to_string();
            if params.get(&name).is_none_or(|ty| ty != "u64") {
                return Err("expected a full-width slot parameter".into());
            }
            Ok(format!("(local.get ${name})"))
        }
        Expr::Cast(c) if type_name(&c.ty)? == "u64" => {
            let inner = if let Expr::Cast(pointer) = &*c.expr {
                if type_name(&pointer.ty)? != "usize" {
                    return Err("unexpected pointer cast".into());
                }
                &*pointer.expr
            } else {
                &*c.expr
            };
            if let Expr::Path(p) = inner {
                let name = p.path.get_ident().ok_or("qualified parameter")?.to_string();
                if !params.contains_key(&name) {
                    return Err("unknown slot parameter".into());
                }
                Ok(format!("(i64.extend_i32_u (local.get ${name}))"))
            } else {
                Err("unsupported argument lowering".into())
            }
        }
        _ => Err(format!("unsupported slot: {expr:?}").into()),
    }
}

fn generate_call(
    function: &syn::ItemFn,
    expected: &[(&str, &str)],
    values: &BTreeMap<String, i64>,
    wat: &mut String,
) -> Result<()> {
    let name = function.sig.ident.to_string();
    let mut params = BTreeMap::new();
    let mut order = Vec::new();
    for (i, arg) in function.sig.inputs.iter().enumerate() {
        let FnArg::Typed(arg) = arg else { return Err("method unsupported".into()) };
        let Pat::Ident(ident) = &*arg.pat else {
            return Err("parameter pattern unsupported".into());
        };
        let n = ident.ident.to_string();
        let ty = type_name(&arg.ty)?;
        if i == 0 {
            if n != "context" || ty != "*mut Context" {
                return Err("unexpected borrowed Context".into());
            }
        } else {
            order.push((n.clone(), ty.clone()));
            params.insert(n, ty);
        }
    }
    if order.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect::<Vec<_>>() != expected {
        return Err(format!("{name}: signature changed; review the marshalling annotation").into());
    }
    if !matches!(&function.sig.output,syn::ReturnType::Type(_,ty) if type_name(ty).is_ok_and(|t|t=="i64"))
    {
        return Err("unexpected syscall result type".into());
    }
    // Wrappers forward the full signed syscall result. Reject any additional
    // work so changes to the Rust boundary require explicit lowering review.
    if !matches!(function.block.stmts.as_slice(),[Stmt::Expr(Expr::Unsafe(u),None)] if matches!(u.block.stmts.as_slice(),[Stmt::Expr(Expr::Call(_),None)]))
    {
        return Err(format!("{name}: wrapper body changed; review scalar lowering").into());
    }
    let mut visitor = Invocation::default();
    visitor.visit_block(&function.block);
    if visitor.calls.len() != 1 {
        return Err("expected exactly one invoke".into());
    }
    let args = &visitor.calls[0].args;
    if args.len() != 3 || !matches!(&args[0],Expr::Path(p) if p.path.is_ident("context")) {
        return Err("invoke shape changed".into());
    }
    let Expr::Cast(op) = &args[1] else { return Err("opcode cast missing".into()) };
    let Expr::Path(path) = &*op.expr else { return Err("opcode path missing".into()) };
    if type_name(&op.ty)? != "u32" {
        return Err("opcode width changed".into());
    }
    let variant = path.path.segments.last().ok_or("opcode missing")?.ident.to_string();
    let opcode = values.get(&format!("Opcode::{variant}")).ok_or("unknown opcode")?;
    let Expr::Array(slots) = &args[2] else { return Err("slot array missing".into()) };
    if slots.elems.len() != 6 {
        return Err("expected six slots".into());
    }
    writeln!(
        wat,
        "    (func (export \"{name}\") {} (result i64)",
        order
            .iter()
            .map(|(n, ty)| {
                let width = if ty == "u64" { "i64" } else { "i32" };
                format!("(param ${n} {width})")
            })
            .collect::<Vec<_>>()
            .join(" ")
    )?;
    writeln!(wat, "        (call $syscall (i32.const {opcode})")?;
    for slot in &slots.elems {
        writeln!(wat, "            {}", lower(slot, &params)?)?;
    }
    writeln!(wat, "        ))")?;
    Ok(())
}

fn run() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 3 {
        return Err("usage: playbit-ocaml-bindgen API_DIRECTORY OUTPUT_DIRECTORY".into());
    }
    let api = Path::new(&args[1]);
    let out = Path::new(&args[2]);
    fs::create_dir_all(out)?;
    let parse = |name: &str| -> Result<syn::File> {
        Ok(syn::parse_file(&fs::read_to_string(api.join(name))?)?)
    };
    let syscall = parse("src/abi/syscall.rs")?;
    let input = parse("src/abi/input.rs")?;
    let env = parse("src/abi/environment.rs")?;
    let ops = parse("src/abi/operations.rs")?;
    let ui = parse("src/ui/mod.rs")?;
    let mut ml = String::from("(* Generated from playbit-api. Do not edit. *)\n");
    let mut values = BTreeMap::new();
    enums(&syscall, &["Opcode", "Error", "EventKind"], &mut ml, &mut values)?;
    enums(&input, &["Key", "GamepadButton", "GamepadAxis"], &mut ml, &mut values)?;
    enums(&env, &["HostLimit", "HostPlatform"], &mut ml, &mut values)?;
    enums(&ui, &["TextStyle", "ButtonStyle", "ThemeColor"], &mut ml, &mut values)?;
    window_style_flags(&ui, &mut ml, &mut values)?;
    for item in syscall.items.iter().chain(&ui.items) {
        if let Item::Const(c) = item {
            let name = c.ident.to_string();
            if [
                "MAX_CALL_BYTES",
                "MAX_DATA_BYTES",
                "MAX_EVENT_BYTES",
                "MAX_RANDOM_BYTES",
                "MAX_QUEUED_EVENTS",
                "VIEW_COLUMN_START",
                "VIEW_COLUMN_END",
                "VIEW_TEXT",
                "VIEW_BUTTON",
                "VIEW_ROW_START",
                "VIEW_ROW_END",
                "VIEW_CANVAS",
                "VIEW_THEME",
                "VIEW_WINDOW_STYLE",
                "VIEW_WINDOW_INPUT",
                "VIEW_WINDOW_SIZE_CONSTRAINTS",
                "VIEW_POPUP_BUTTON",
                "VIEW_CONTEXT_MENU_START",
                "VIEW_CONTEXT_MENU_END",
                "MENU_ACTION",
                "CANVAS_FLAG_CONTINUOUS",
                "CANVAS_FLAG_VIEWPORT",
                "CANVAS_FLAG_WINDOW",
                "CANVAS_FLAG_CLEAR",
                "MAX_CANVAS_EDGE",
                "MAX_WINDOW_CANVAS_EDGE",
                "MAX_CANVAS_ITEMS",
                "MAX_CANVAS_TEXT_BYTES",
                "MAX_CANVAS_PIXEL_BYTES",
                "MAX_CANVAS_FONT_SIZE",
                "CONTROL_DISABLED",
            ]
            .contains(&name.as_str())
            {
                let n = integer(&c.expr, &values)?;
                values.insert(name.clone(), n);
                writeln!(ml, "let {} = {n}", name.to_lowercase())?;
            }
            if name == "VIEW_MAGIC" {
                if let Expr::Lit(l) = &*c.expr {
                    if let syn::Lit::ByteStr(s) = &l.lit {
                        writeln!(ml, "let view_magic = {:?}", String::from_utf8(s.value())?)?;
                    }
                } else {
                    return Err("VIEW_MAGIC shape changed".into());
                }
            }
        }
    }
    // repr(C) layout is also checked by the separately compiled wasm32 fixture.
    let event = ops
        .items
        .iter()
        .find_map(|i| {
            if let Item::Struct(s) = i {
                if s.ident == "Event" { Some(s) } else { None }
            } else {
                None
            }
        })
        .ok_or("Event missing")?;
    let fields = event
        .fields
        .iter()
        .map(|f| Ok((f.ident.as_ref().ok_or("unnamed field")?.to_string(), type_name(&f.ty)?)))
        .collect::<Result<Vec<_>>>()?;
    if fields
        != [
            ("size".into(), "u32".into()),
            ("written".into(), "u32".into()),
            ("generation".into(), "u64".into()),
            ("kind".into(), "u32".into()),
            ("target".into(), "u32".into()),
            ("object_id".into(), "u32".into()),
        ]
    {
        return Err("Event layout changed; review marshalling".into());
    }
    const EVENT_BYTES: i64 = 32;
    writeln!(ml, "let event_bytes = {EVENT_BYTES}")?;
    ml.push_str(
        "let event_size_offset = 0\n\
         let event_written_offset = 4\nlet event_generation_offset = 8\n\
         let event_kind_offset = 16\nlet event_target_offset = 20\nlet event_object_id_offset = 24\n",
    );
    let mut wat = String::from(
        ";; Generated scalar lowering from Rust. Buffer ownership lives in playbit_ffi.wat.\n(module\n    (import \"playbit\" \"pb_syscall\" (func $syscall (param i32 i64 i64 i64 i64 i64 i64) (result i64)))\n",
    );
    // Structured adapters own pointer layouts and bounded buffer copies. Their
    // scalar transport retains complete 64-bit handles and uses generated
    // Opcode values, without a second operation table in the compiler fork.
    wat.push_str(
        "    (func (export \"pb_call_slots\") (param $opcode i32) (param $a i64) \
         (param $b i64) (param $c i64) (param $d i64) (param $e i64) (param $f i64) \
         (result i64)\n        (call $syscall (local.get $opcode) (local.get $a) \
         (local.get $b) (local.get $c) (local.get $d) (local.get $e) (local.get \
         $f)))\n",
    );
    for &(name, annotation) in OPERATIONS {
        let function = ops
            .items
            .iter()
            .chain(&env.items)
            .find_map(|i| {
                if let Item::Fn(f) = i {
                    if f.sig.ident == name { Some(f) } else { None }
                } else {
                    None
                }
            })
            .ok_or_else(|| format!("missing operation {name}"))?;
        generate_call(function, annotation, &values, &mut wat)?;
    }
    wat.push_str(")\n");
    fs::write(out.join("playbit_generated.ml"), &ml)?;
    // Classify the generated literals for the explicit interface.
    let mut interface = String::from("(* Generated from playbit-api. *)\n");
    for line in ml.lines() {
        if let Some(name) = line.strip_prefix("module ").and_then(|s| s.strip_suffix(" = struct")) {
            writeln!(interface, "module {name} : sig")?
        } else if line == "end" {
            interface.push_str("end\n")
        } else if let Some(rest) = line.trim().strip_prefix("let ") {
            let (name, value) = rest.split_once(" = ").ok_or("generated declaration")?;
            writeln!(
                interface,
                "    val {name} : {}",
                if value.starts_with('"') {
                    "string"
                } else if value.ends_with('l') {
                    "int32"
                } else {
                    "int"
                }
            )?;
        }
    }
    fs::write(out.join("playbit_generated.mli"), interface)?;
    fs::write(out.join("playbit_raw.wat"), wat)?;
    let metadata = format!(
        "{{{}}}\n",
        values
            .iter()
            .map(|(name, value)| format!("\"{name}\":{value}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    fs::write(out.join("playbit_abi.json"), metadata)?;
    let mut ffi = include_str!("../../playbit_ffi.wat").to_owned();
    values.insert("EVENT_BYTES".into(), EVENT_BYTES);
    // Offset zero is the null pointer. The aligned header starts at byte eight.
    values.insert("EVENT_DATA_OFFSET".into(), 8 + EVENT_BYTES);
    for name in ["MAX_CALL_BYTES", "MAX_EVENT_BYTES", "EVENT_BYTES", "EVENT_DATA_OFFSET"] {
        ffi = ffi.replace(&format!("@{name}@"), &values[name].to_string());
    }
    fs::write(out.join("playbit_ffi.wat"), ffi)?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("binding generation failed: {error}");
        std::process::exit(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_lowering_preserves_unsigned_slots_and_rejects_unreviewed_bodies() {
        // Derive slots from the wrapper, then reject new work and changed signatures.
        let source = r#"pub unsafe fn pb_log(context: *mut Context, text: *const u8, length: u32) -> i64 {
            unsafe { super::invocation::invoke(context, super::syscall::Opcode::Log as u32,
                [text as usize as u64, length as u64, 0, 0, 0, 0]) }
        }"#;
        let values = BTreeMap::from([
            ("Opcode::Log".into(), playbit_api::abi::syscall::Opcode::Log as i64),
            ("Error::Internal".into(), playbit_api::abi::syscall::Error::Internal as i64),
        ]);
        let function = syn::parse_str::<syn::ItemFn>(source).expect("fixture function");
        let mut wat = String::new();
        generate_call(&function, OPERATIONS[0].1, &values, &mut wat).expect("supported wrapper");
        assert!(wat.contains("i64.extend_i32_u (local.get $text)"));
        assert!(wat.contains("i64.extend_i32_u (local.get $length)"));
        for changed in [
            source.replace("length: u32", "length: u64"),
            source.replace("unsafe {", "let checked = true; unsafe {"),
            source.replace("-> i64", "-> i32"),
            source.replace("0, 0, 0, 0])", "0, 0, 0, 0]) as i32"),
        ] {
            let function = syn::parse_str::<syn::ItemFn>(&changed).expect("changed Rust fixture");
            assert!(
                generate_call(&function, OPERATIONS[0].1, &values, &mut String::new()).is_err()
            );
        }
    }
}
