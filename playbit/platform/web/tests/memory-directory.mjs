// In-memory directory ownership for publication tests; durability tests also
// reopen the production journal and separately exercise actual web browser OPFS.
export class Directory {
    entries = new Map();
    async *values() {
        for (const [name, entry] of this.entries) {
            yield { name, kind: entry instanceof Directory ? 'directory' : 'file' };
        }
    }
    async getDirectoryHandle(name, { create = false } = {}) {
        let entry = this.entries.get(name);
        if (!entry && create) this.entries.set(name, entry = new Directory());
        if (!entry) throw new DOMException('Missing directory', 'NotFoundError');
        if (!(entry instanceof Directory)) throw new DOMException('Expected directory', 'TypeMismatchError');
        return entry;
    }
    async getFileHandle(name, { create = false } = {}) {
        let entry = this.entries.get(name);
        if (!entry && create) this.entries.set(name, entry = { name, kind: 'file' });
        if (!entry) throw new DOMException('Missing file', 'NotFoundError');
        if (entry instanceof Directory) throw new DOMException('Expected file', 'TypeMismatchError');
        return entry;
    }
    async removeEntry(name, { recursive = false } = {}) {
        const entry = this.entries.get(name);
        if (!entry) throw new DOMException('Missing entry', 'NotFoundError');
        if (entry instanceof Directory && entry.entries.size && !recursive) throw new DOMException('Directory is not empty', 'InvalidModificationError');
        this.entries.delete(name);
    }
}
