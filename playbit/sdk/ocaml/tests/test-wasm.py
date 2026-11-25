#!/usr/bin/env python3
"""Target contract, ABI marshalling and independent wasm32 Rust layout checks."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parent.parent
repo = root.parents[2]
compiler = Path(os.environ.get("JS_OF_OCAML_ROOT", root / "_build/js_of_ocaml")) / "_build/default/compiler/bin-wasm_of_ocaml/wasm_of_ocaml.exe"
env = os.environ.copy()
env["OPAMROOT"] = str(root / "_build/opam")
env["PATH"] = str(root / "_build/binaryen-version_133/bin") + ":" + (root / "_build/compiler-bin").read_text().strip() + ":" + env["PATH"]
env["SCCACHE_SERVER_UDS"] = str(repo / "_sccache.sock")

def run(args, **options):
    return subprocess.run([str(a) for a in args], cwd=root, env=env, check=True, **options)

def compile_program(directory, source, success=True):
    (directory / "probe.ml").write_text(source)
    run(["opam", "exec", "--switch=playbit", "--", "ocamlc", "-no-check-prims", "-I", "_build/wasm/debug", "_build/wasm/debug/playbit_generated.cmo", "_build/wasm/debug/playbit.cmo", directory / "probe.ml", "-o", directory / "probe.byte"])
    command = [compiler, "--target", "playbit", "-o", directory / "probe.wasm", "_build/generated/playbit_raw.wat", "_build/generated/playbit_ffi.wat", directory / "probe.byte"]
    result = subprocess.run([str(a) for a in command], cwd=root, env=env, text=True, capture_output=True)
    if (result.returncode == 0) != success:
        raise AssertionError(result.stdout + result.stderr)
    if not success:
        assert "unsupported Playbit primitive" in result.stderr, result.stderr

with tempfile.TemporaryDirectory(prefix="contract-", dir=root / "_build") as temporary:
    directory = Path(temporary)
    compile_program(directory, r'''
open Playbit
external raw_poll : bytes -> bytes -> int64 = "caml_pb_event_poll"
external channels : unit -> out_channel list = "caml_ml_out_channels_list"
let turns = ref 0
let host_error expected call =
    match call () with
    | exception Host_error code when code = Int32.of_int expected -> ()
    | _ -> failwith "syscall error width"
let () =
    if host_platform () <> 1l || host_limit 0l <> Int32.max_int then
        failwith "scalar syscall result";
    host_error Error.internal (fun () -> host_limit 1l);
    host_error Error.internal (fun () -> host_limit 2l);
    host_error Error.access_denied (fun () -> host_limit 3l);
    let random = Bytes.make 32 'a' in
    random_fill random;
    if random <> Bytes.make 32 '\007' then failwith "random output copy";
    Bytes.fill random 0 32 'b';
    host_error Error.unavailable (fun () -> random_fill random);
    if random <> Bytes.make 32 'b' then failwith "failed random output copy";
    host_error Error.limit_exceeded (fun () -> random_fill (Bytes.create (max_random_bytes + 1)));
    random_fill Bytes.empty;
    if poll_deadline (Int64.add (monotonic_ns ()) 50_000L) <> None then
        failwith "deadline should have expired";
    (match poll_deadline Int64.minus_one with
     | Some event when event.kind = Int32.of_int EventKind.data_changed
                       && event.object_id = 0l && Bytes.equal event.data (Bytes.make 256 'd') -> ()
     | _ -> failwith "deadline size probe lost event");
    if poll_deadline 0L <> None then failwith "nonblocking deadline poll";
    if Obj.tag (Obj.repr stdout) <> Obj.custom_tag
       || List.length (channels ()) <> 2 then failwith "standard channel representation";
    flush stdout;
    (try ignore (raw_poll (Bytes.create 23) (Bytes.create 1)); failwith "accepted bad header"
     with Invalid_argument _ -> log "bad header rejected");
    log "init";
    run (fun () ->
        incr turns;
        if !turns = 1 then begin
            for _ = 1 to 2 do
                (try ignore (poll ()); failwith "accepted malformed trace configuration"
                 with Failure message when message = "Playbit trace configuration is malformed" -> log "configuration rejected")
            done;
            match poll () with
            | None -> failwith "missing event"
            | Some event ->
                if Int64.compare event.generation Int64.min_int <> 0
                   || Int32.compare event.kind (-1l) <> 0
                   || Int32.compare event.target Int32.min_int <> 0
                   || event.object_id <> 123l
                   || Bytes.length event.data <> 512
                   || Bytes.get event.data 511 <> 'x' then failwith "event bits changed";
                (try failwith "caught" with Failure _ -> log "caught")
        end;
        log ("turn " ^ string_of_int !turns);
        if !turns = 2 then begin
            (try ignore (poll ()); failwith "accepted mandatory unknown"
             with Host_error code when code = Int32.of_int Error.unsupported -> log "mandatory rejected");
            (try ignore (poll ()); failwith "retried after terminal event"
             with Host_error code when code = Int32.of_int Error.unsupported -> ());
            failwith "uncaught"
        end)
''')
    # This unit boundary host inspects exact ABI slots and copies; the browser
    # and native integration tests separately use the production host adapters.
    js = r'''
import fs from 'node:fs';
const [wasm, metadata, raw] = process.argv.slice(2);
const abi = JSON.parse(fs.readFileSync(metadata));
const assert = (value, message) => { if (!value) throw Error(message); };
const module = new WebAssembly.Module(fs.readFileSync(wasm));
assert(WebAssembly.Module.imports(module).length===1, 'one import');
assert(JSON.stringify(WebAssembly.Module.exports(module).map(e=>e.name).sort())===JSON.stringify(['memory','pb_main']), 'exports');
let instance, calls=0, configurations=0, delivered=false, randomCalls=0; const logs=[];
const deadlines=[];
instance = new WebAssembly.Instance(module, {playbit:{pb_syscall(op,...args) {
    if (op===abi['Opcode::ClockRead']) return 1n<<50n;
    if (op===abi['Opcode::EventPollDeadline']) {
        deadlines.push(BigInt.asUintN(64,args[3]));
        assert(args[0]===8n && args[1]===40n && args[4]===0n && args[5]===0n, 'deadline poll slots');
        if (deadlines.length===1 || deadlines.length===4) return -6n;
        if (args[2]<256n) return 256n;
        const data=new DataView(instance.exports.memory.buffer), header=Number(args[0]);
        data.setUint32(header+4,24,true);data.setBigUint64(header+8,7n,true);
        data.setUint32(header+16,abi['EventKind::DataChanged'],true);data.setUint32(header+20,0,true);
        new Uint8Array(instance.exports.memory.buffer,Number(args[1]),256).fill(100);
        return 256n;
    }
    if (op===abi['Opcode::RandomFill']) {
        randomCalls++;
        assert(args[0]===80n && args.slice(2).every(x=>x===0n), 'random scalar slots');
        assert(args[1]===(randomCalls===3 ? 0n : 32n), 'random output size');
        new Uint8Array(instance.exports.memory.buffer,Number(args[0]),Number(args[1])).fill(7);
        return randomCalls===2 ? -5n : 0n;
    }
    if (op===abi['Opcode::HostPlatform']) return 1n;
    if (op===abi['Opcode::HostLimit']) return [2147483647n,0x7edcba9880000001n,-0x8000000000000000n,-2n][Number(args[0])];
    if (op===abi['Opcode::Log']) { logs.push(new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,Number(args[0]),Number(args[1]))));return 0n; }
    if (op===abi['Opcode::EventPoll']) {
        calls++;
        if (configurations<4) {
            const config=configurations++, data=new DataView(instance.exports.memory.buffer), header=Number(args[0]);
            data.setUint32(header+4,24,true);data.setBigUint64(header+8,7n,true);
            data.setUint32(header+16,abi['EventKind::TraceConfig'],true);
            data.setUint32(header+20,config===3 ? 1 : 0,true);
            data.setBigUint64(Number(args[1]),config===1 ? 17n : 0n,true);
            return config===2 ? 7n : 8n;
        }
        if (delivered) {
            const data=new DataView(instance.exports.memory.buffer), header=Number(args[0]);
            data.setUint32(header+4,24,true);data.setBigUint64(header+8,7n,true);
            data.setUint32(header+16,999,true);data.setUint32(header+20,0,true);
            return 0n;
        }
        if (args[2]<512n) return 512n;
        const data=new DataView(instance.exports.memory.buffer), header=Number(args[0]);
        assert(data.getUint32(header,true)===32 && data.getUint32(header+4,true)===0, 'caller record capacity');
        data.setUint32(header+4,28,true);data.setUint32(header+24,123,true);data.setBigUint64(header+8, 0x8000000000000000n, true);data.setUint32(header+16,0xffffffff,true);data.setUint32(header+20,0x80000000,true);
        new Uint8Array(instance.exports.memory.buffer,Number(args[1]),512).fill(120);delivered=true;return 512n;
    }
    throw Error('Unexpected syscall '+op);
}}});
instance.exports.pb_main(); assert(calls===6&&configurations===4&&delivered, 'trace configuration snapshots and capacity retry');
assert(deadlines.length===4 && deadlines[0]===(1n<<50n)+50000n && deadlines[1]===(1n<<64n)-1n
    && deadlines[2]===deadlines[1] && deadlines[3]===0n, 'full deadline survives size probe and retry');
assert(randomCalls===3, 'random buffer copies, errors and local limit');
assert(logs.join('|')==='bad header rejected|init|configuration rejected|configuration rejected|caught|turn 1', 'first turn: '+logs);
let trapped=false;try {instance.exports.pb_main();}catch {trapped=true;}assert(trapped, 'uncaught exception traps');
assert(logs.includes('mandatory rejected') && calls===7, 'mandatory unknown fails explicitly and never polls again');
trapped=false;try {instance.exports.pb_main();}catch {trapped=true;}assert(trapped, 'failed turn cannot be reentered');
assert(logs.filter(x=>x==='init').length===1,'initialize once');
let result=0n, slots;
const rawModule=new WebAssembly.Module(fs.readFileSync(raw));
const rawInstance=new WebAssembly.Instance(rawModule,{playbit:{pb_syscall(op,...args){assert(op===abi['Opcode::Log'],'generated opcode');slots=args;return result;}}});
rawInstance.exports.pb_log(-1,0x80000000);assert(slots[0]===0xffffffffn&&slots[1]===0x80000000n&&slots.slice(2).every(x=>x===0n),'unsigned widths and unused slots');
for(const value of [-2147483648n,2147483647n,2147483648n,-2147483649n,0x7edcba9880000001n,-0x8000000000000000n]) {
    result=value;assert(rawInstance.exports.pb_log(1,2)===value, 'full signed syscall result');
}
console.log('Target contract, initialization, exceptions, trace configuration, buffer validation, retry and integer widths passed');
'''
    (directory / "check.mjs").write_text(js)
    run(["wasm-tools", "parse", "_build/generated/playbit_raw.wat", "-o", directory / "raw.wasm"])
    run(["node", directory / "check.mjs", directory / "probe.wasm", "_build/generated/playbit_abi.json", directory / "raw.wasm"])
    for source in ['let () = Playbit.log (string_of_float (Sys.time ()))', 'let () = print_endline \"unsupported stdio\"']:
        compile_program(directory, source, success=False)

with tempfile.TemporaryDirectory(prefix="expanded-", dir=root / "_build") as temporary:
    directory = Path(temporary)
    compile_program(directory, (root / "tests/expanded.ml").read_text())
    run(["node", "tests/check-expanded.mjs", directory / "probe.wasm", "_build/generated/playbit_abi.json"])

layout_env = dict(env, CARGO_TARGET_DIR=str(root / "_build/layout-target"))
subprocess.run(["cargo", "build", "--locked", "--manifest-path", str(repo / "playbit-api/tests/wasm-layout/Cargo.toml"), "--target", "wasm32-unknown-unknown"], cwd=root, env=layout_env, check=True)
run(["node", "--input-type=module", "-e", """
import fs from 'node:fs';
const module=new WebAssembly.Module(fs.readFileSync('_build/layout-target/wasm32-unknown-unknown/debug/playbit_wasm_layout.wasm'));
const sections=WebAssembly.Module.customSections(module,'playbit.abi.layout');
if(sections.length!==1||JSON.stringify([...new Uint32Array(sections[0])])!==JSON.stringify([32,8,8,16,20,24]))throw Error('wasm32 Event layout changed');
console.log('wasm32 Rust Event layout passed');
"""])
