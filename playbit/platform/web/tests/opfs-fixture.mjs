import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { OpfsResources } from "../src/opfs-store.ts";

// Native files implement the synchronous OPFS handle surface for deterministic
// byte/layout regressions. They do not establish web browser durability or RSS.
export class DiskFile {
    constructor(filename, create = true) { this.fd = fs.openSync(filename, create ? "w+" : "r+", 0o600); this.maxRead = 0; this.maxWrite = 0; this.failFlush = false; }
    getSize() { return fs.fstatSync(this.fd).size; }
    read(out, { at }) { this.maxRead = Math.max(this.maxRead, out.length); return fs.readSync(this.fd, out, 0, out.length, at); }
    write(bytes, { at }) { this.maxWrite = Math.max(this.maxWrite, bytes.length); return fs.writeSync(this.fd, bytes, 0, bytes.length, at); }
    truncate(length) { fs.ftruncateSync(this.fd, length); }
    flush() { if (this.failFlush) { this.failFlush = false; throw new Error("injected flush failure"); } fs.fsyncSync(this.fd); }
    close() { if (this.fd !== undefined) { fs.closeSync(this.fd); this.fd = undefined; } }
}
export function fixture() {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), "playbit-opfs-stream-"));
    let sequence = 0;
    const file = (name = `file-${sequence++}`, create = true) => new DiskFile(path.join(directory, name), create);
    const resources = new OpfsResources([file(), file()], [file(), file()]);
    return { resources, file, cleanup() { resources.close(); fs.rmSync(directory, { recursive: true, force: true }); } };
}
export const bytes = text => new TextEncoder().encode(text);
