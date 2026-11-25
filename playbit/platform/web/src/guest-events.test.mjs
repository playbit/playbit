import { readFile } from "node:fs/promises";
import { guestEventCases } from "../tests/guest-event-fixture.mjs";

const bytes = await readFile(new URL("../../../../target/wasm-lowering/events-rust.wasm", import.meta.url));
guestEventCases(await WebAssembly.compile(bytes), "current compiled SDK", 32);
