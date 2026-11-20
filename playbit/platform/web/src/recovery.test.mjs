import { encodeId } from './identifier.ts';
import { createHash } from "node:crypto";
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { RecoveryClient, seal, open, mergeCatalog } from "./recovery.ts";
import { parseAccountKey, formatAccountKey } from "./account-key.ts";
import { recoveryMemory } from "../tests/recovery-memory.mjs";
import { deriveSessionKeys } from "./invitation.ts";
const encode = value => new TextEncoder().encode(value);
const account = encodeId(new Uint8Array(16).fill(0xab));
const fixture = JSON.parse(await readFile(new URL("../../../tests/fixtures/recovery-v1.json", import.meta.url), "utf8"));
const storage = recoveryMemory;
const unlocked = callback => callback();
function service() {
    let vault = { revision: 0, ciphertext: null }, operation;
    const ap = { requests: [], failAfterCommit: false, beforePut: null,
        async request(request) {
            this.requests.push(structuredClone(request));
            if (request.op === "get_credential_vault") return { data: structuredClone(vault) };
            if (this.beforePut) { const callback = this.beforePut; this.beforePut = null; await callback(); }
            if (request.operation_id === operation) {
                assert.equal(request.ciphertext, vault.ciphertext);
                assert.equal(request.expected_revision + 1, vault.revision);
            } else {
                if (request.expected_revision !== vault.revision) throw new Error("credential backup changed");
                vault = { revision: vault.revision + 1, ciphertext: request.ciphertext }; operation = request.operation_id;
            }
            if (this.failAfterCommit) { this.failAfterCommit = false; throw new Error("lost response"); }
            return { data: structuredClone(vault) };
        },
    };
    return ap;
}
async function invitation(number) {
    const seed = new Uint8Array(16).fill(number);
    const { session_id } = await deriveSessionKeys(seed);
    const root = { id: encodeId(Buffer.from(number.toString(16).padStart(32, "0"), 'hex')), owner: account, name: `App ${number}`, share_key_with_service: true, personal: false };
    return { seed: Array.from(seed),
        session: { id: session_id, root_volume: root.id, creator: account, app: { publisher: account, app_id: `app-${number}` },
            selector: { kind: "exact", build_id: encodeId(new Uint8Array(32).fill(0x11)) }, resolved_build_id: encodeId(new Uint8Array(32).fill(0x11)), update_authority: "creator" },
        root, root_credentials: { database_key: Array(32).fill(number), blob_key: Array(32).fill(number + 1) },
        };
}

test("web decrypts native AES-GCM fixture and authenticates account/revision/secret/bytes", async () => {
    const secret = Uint8Array.from(Buffer.from(fixture.secret, "hex"));
    assert.equal(new TextDecoder().decode(await open(fixture.account, fixture.revision, secret, fixture.ciphertext)), fixture.plaintext);
    assert.deepEqual(parseAccountKey(formatAccountKey(secret.slice(0,16))), secret.slice(0,16));
    const encrypted = await seal(account, 3, secret, encode("private keys"));
    assert.notEqual(encrypted, await seal(account, 3, secret, encode("private keys")));
    await assert.rejects(open(encodeId(new Uint8Array(16).fill(0xcd)), 3, secret, encrypted));
    await assert.rejects(open(account, 4, secret, encrypted));
    await assert.rejects(open(account, 3, new Uint8Array(32), encrypted));
    const bytes = Buffer.from(encrypted, "base64url");
    for (let index = 0; index < bytes.length; index++) {
        const damaged = Buffer.from(bytes); damaged[index] ^= 1;
        await assert.rejects(open(account, 3, secret, damaged.toString("base64url")));
    }
    await assert.rejects(open(account, 3, secret, `${encrypted}=`));
    await assert.rejects(seal(account, 1, secret, new Uint8Array(2 * 1024 * 1024 + 1)));
});

test("secret save is explicit, encrypted upload replay survives reopening, sign-in alone stays locked", async () => {
    const ap = service(), disk = storage(), client = new RecoveryClient(ap, account, disk, unlocked);
    assert.equal((await client.status()).kind, "setup");
    const secret = await client.prepare();
    assert.equal(await client.prepare(), secret);
    assert.equal(ap.requests.filter(request => request.op === "put_credential_vault").length, 0);
    ap.failAfterCommit = true;
    await assert.rejects(client.save(), /lost response/);
    assert.equal((await client.status()).pending_save, true);
    const reopened = new RecoveryClient(ap, account, disk, unlocked);
    await reopened.backup();
    assert.deepEqual(await reopened.status(), { kind: "ready", revision: 1, pending_save: false });
    const uploads = ap.requests.filter(request => request.op === "put_credential_vault");
    assert.equal(uploads[0].operation_id, uploads[1].operation_id);
    assert.equal(uploads[0].ciphertext, uploads[1].ciphertext);
    assert(!JSON.stringify(uploads).includes(secret));
    const clean = new RecoveryClient(ap, account, storage(), unlocked);
    assert.equal((await clean.status()).kind, "locked");
    await assert.rejects(clean.prepare());
    await assert.rejects(clean.save());
    await assert.rejects(clean.recover(formatAccountKey(new Uint8Array(16))));
    assert.deepEqual(await clean.catalog(), { account, volumes: [], invitations: [] });
    await clean.recover(secret);
    assert.equal((await clean.status()).kind, "ready");
});

test('a backup ACK cannot overwrite a newer local catalog; retry preserves the exact pending upload', async () => {
    const ap = service(), disk = storage(), first = new RecoveryClient(ap, account, disk, unlocked);
    await first.prepare();
    const second = new RecoveryClient(ap, account, disk, unlocked);
    ap.beforePut = async () => {
        const snapshot = await second.snapshot();
        snapshot.state.catalog.volumes.push(personal(30));
        await second.store(snapshot);
    };
    await assert.rejects(first.save(), /Account state changed/);
    const interrupted = await second.state(), pending = structuredClone(interrupted.pending);
    assert.equal(interrupted.revision, 0);
    assert.deepEqual(interrupted.catalog.volumes, [personal(30)]);
    assert.ok(pending);
    await first.backup();
    const uploads = ap.requests.filter(request => request.op === 'put_credential_vault');
    assert.equal(uploads[0].operation_id, uploads[1].operation_id);
    assert.equal(uploads[0].ciphertext, uploads[1].ciphertext);
    const after = await first.state();
    assert.equal(after.revision, 2);
    assert.equal(after.pending, undefined);
    assert.deepEqual(after.catalog.volumes, [personal(30)]);
});

test('a prepared recovery snapshot cannot be written through another account client', async () => {
    const disk = storage(), source = new RecoveryClient({}, account, disk, unlocked);
    const other = new RecoveryClient({}, encodeId(new Uint8Array(16).fill(0xcd)), disk, unlocked);
    await assert.rejects(other.store(await source.snapshot()), /another account/);
    assert.equal((await other.snapshot()).control.revision, 0n);
});

test("concurrent device backups merge capabilities and restored catalogs rebuild invitations without snapshots", async () => {
    const ap = service(), first = new RecoveryClient(ap, account, storage(), unlocked);
    const secret = await first.prepare(); await first.save();
    const second = new RecoveryClient(ap, account, storage(), unlocked); await second.recover(secret);
    const firstURL = await invitation(1), secondURL = await invitation(5), thirdURL = await invitation(9);
    await first.rememberInvitation(firstURL);
    ap.beforePut = () => first.rememberInvitation(thirdURL);
    await assert.rejects(second.rememberInvitation(secondURL), /changed/);
    await second.backup();
    assert.deepEqual(new Set((await second.state()).catalog.invitations), new Set([firstURL, secondURL, thirdURL]), "merged remote capabilities are retained locally before a catalog refresh");
    assert.equal((await second.status()).pending_save, false, "acknowledgment hashes the merged upload");
    const clean = new RecoveryClient(ap, account, storage(), unlocked); await clean.recover(secret);
    const catalog = await clean.catalog();
    assert.deepEqual(new Set(catalog.invitations), new Set([firstURL, secondURL, thirdURL]));
    assert.equal(catalog.volumes.length, 3);
    assert.deepEqual(Object.keys(catalog).sort(), ["account", "invitations", "volumes"]);
    const changed = structuredClone(catalog); changed.volumes[0].credentials.blob_key[0] ^= 1;
    await assert.rejects(mergeCatalog(changed, catalog), /keys conflict/);
});

function personal(number) {
    return { volume: { id: encodeId(Buffer.from(number.toString(16).padStart(32, "0"), 'hex')), owner: account, name: `Personal ${number}`, share_key_with_service: true, personal: true },
        credentials: { database_key: Array(32).fill(number), blob_key: Array(32).fill(number + 1) } };
}

test("explicit catalog refresh discovers another device’s personal volume without polling unchanged backups", async () => {
    const ap = service(), firstDisk = storage(), first = new RecoveryClient(ap, account, firstDisk, unlocked);
    const secret = await first.prepare(); await first.save();
    const webDisk = storage(), web = new RecoveryClient(ap, account, webDisk, unlocked);
    await web.recover(secret);
    const state = await first.state(); state.catalog.volumes.push(personal(30)); await firstDisk.write(account, (await firstDisk.read(account)).control, state); await first.backup();
    const before = ap.requests.length;
    await web.backup(); await web.backup();
    assert.equal(ap.requests.length, before, "automatic backups do not poll when the local catalog is unchanged");
    assert.deepEqual((await web.catalog()).volumes, [personal(30)]);
    assert.deepEqual((await new RecoveryClient(ap, account, webDisk, unlocked).state()).catalog.volumes, [personal(30)]);
    assert.equal((await web.status()).pending_save, false);
    const refreshed = ap.requests.length;
    await web.backup();
    assert.equal(ap.requests.length, refreshed, "refreshing remote-only changes needs no redundant upload");
});

test("catalog refresh preserves uncertain upload bytes and local additions across a competing newer vault", async () => {
    const ap = service(), disk = storage(), web = new RecoveryClient(ap, account, disk, unlocked);
    const secret = await web.prepare(); await web.save();
    const localURL = await invitation(40), remoteURL = await invitation(45);
    ap.failAfterCommit = true;
    await assert.rejects(web.rememberInvitation(localURL), /lost response/);
    const pending = (await web.state()).pending;
    const other = new RecoveryClient(ap, account, storage(), unlocked); await other.recover(secret);
    await other.rememberInvitation(remoteURL);
    const state = await web.state(); state.catalog.volumes.push(personal(50)); await disk.write(account, (await disk.read(account)).control, state);
    const puts = ap.requests.filter(request => request.op === "put_credential_vault").length;
    const merged = await web.catalog();
    assert.deepEqual(new Set(merged.invitations), new Set([localURL, remoteURL]));
    assert.ok(merged.volumes.some(item => item.volume.id === personal(50).volume.id));
    assert.deepEqual((await web.state()).pending, pending, "read does not replace an uncertain operation or ciphertext");
    assert.equal(ap.requests.filter(request => request.op === "put_credential_vault").length, puts);
    await web.backup();
    const replay = ap.requests.filter(request => request.op === "put_credential_vault")[puts];
    assert.equal(replay.operation_id, pending.operation_id); assert.equal(replay.ciphertext, pending.ciphertext);
    assert.equal((await web.status()).pending_save, false);
    const clean = new RecoveryClient(ap, account, storage(), unlocked); await clean.recover(secret);
    assert.deepEqual((await clean.state()).catalog, (await web.state()).catalog);
});

test("a locked web client can retain a newly accepted invitation without overwriting existing private backup", async () => {
    const ap = service(), existing = new RecoveryClient(ap, account, storage(), unlocked);
    const secret = await existing.prepare(); await existing.save();
    const locked = new RecoveryClient(ap, account, storage(), unlocked), url = await invitation(13);
    await locked.rememberInvitation(url);
    assert.equal((await locked.status()).kind, "locked");
    assert.equal(ap.requests.filter(request => request.op === "put_credential_vault").length, 1);
    await locked.recover(secret); await locked.backup();
    const clean = new RecoveryClient(ap, account, storage(), unlocked); await clean.recover(secret);
    assert.deepEqual((await clean.catalog()).invitations, [url]);
});

test("a delayed recovery fetch cannot lower a newer local backup acknowledgment", async () => {
    const ap = service(), client = new RecoveryClient(ap, account, storage(), unlocked);
    const secret = await client.prepare(); await client.save();
    let release, arrived;
    const gate = new Promise(resolve => { release = resolve; });
    const started = new Promise(resolve => { arrived = resolve; });
    const request = ap.request.bind(ap);
    let paused = false;
    ap.request = async value => {
        const result = await request(value);
        if (value.op === "get_credential_vault" && !paused) { paused = true; arrived(); await gate; }
        return result;
    };
    const restored = client.recover(secret);
    await started;
    await assert.rejects(client.recover(secret), /already in progress/);
    await client.rememberInvitation(await invitation(55));
    const state = await client.state();
    assert.equal(state.revision, 2);
    release();
    await assert.rejects(restored, /older than this web browser/);
    assert.deepEqual(await client.state(), state);
    await client.recover(secret);
    assert.equal((await client.state()).revision, 2);
});


test("account deletion survives stale vault refresh and credential recovery", async () => {
    const ap = service(), disk = storage(), client = new RecoveryClient(ap, account, disk, unlocked);
    const secret = await client.prepare(); await client.save();
    const invite = await invitation(4);
    await client.rememberInvitation(invite); await client.backup();
    const snapshot = await client.snapshot();
    snapshot.state.deleted_volumes = [invite.root.id];
    await client.store(snapshot);
    assert(!(await client.catalog()).volumes.some(item => item.volume.id === invite.root.id));
    await client.recover(secret);
    assert(!(await client.catalog()).volumes.some(item => item.volume.id === invite.root.id));
    assert.equal((await client.catalog()).invitations.length, 0);
    await client.backup();
    assert.equal((await client.status()).pending_save, false);
});

test("256-volume catalog and retained invitations survive sealing and merge", async () => {
    const hash = label => createHash("sha256").update(label).digest();
    const invitations = await Promise.all(Array.from({length: 256}, async (_, n) => {
        const item = await invitation(n);
        item.seed = Array.from(hash(`seed-${n}`).subarray(0,16));
        item.session.id = (await deriveSessionKeys(Uint8Array.from(item.seed))).session_id;
        item.root.id = encodeId(hash(`id-${n}`).subarray(0,16));
        item.session.root_volume = item.root.id;
        item.root.name = Array.from({length: 6}, (_, block) => hash(`name-${n}-${block}`).toString("base64url")).join("").slice(0,256);
        item.root_credentials = {database_key: Array.from(hash(`database-${n}`)), blob_key: Array.from(hash(`blob-${n}`))};
        return item;
    }));
    const catalog = {account, volumes: invitations.map(item => ({volume:item.root, credentials:item.root_credentials})), invitations};
    const merged = await mergeCatalog({account, volumes:[], invitations:[]}, catalog, account);
    const plaintext = encode(JSON.stringify(merged));
    assert.ok(plaintext.length > 128 * 1024);
    const secret = new Uint8Array(32).fill(9);
    const ciphertext = await seal(account, 1, secret, plaintext);
    assert.ok(Buffer.from(ciphertext, "base64url").length > 128 * 1024);
    assert.deepEqual(JSON.parse(new TextDecoder().decode(await open(account, 1, secret, ciphertext))), merged);
});

// Frozen Node WebCrypto/zlib v2 input; the native reader consumes the same fixture.
test("v2 fixture retains v1 reads and authenticates the compression version", async () => {
    const value = JSON.parse(await readFile(new URL("../../../tests/fixtures/recovery-v2.json", import.meta.url), "utf8"));
    const secret = Uint8Array.from(Buffer.from(value.secret, "hex"));
    assert.equal(new TextDecoder().decode(await open(value.account, value.revision, secret, value.ciphertext)), value.pattern.repeat(value.repetitions));
    const altered = Buffer.from(value.ciphertext, "base64url"); altered[0] = 1;
    await assert.rejects(open(value.account, value.revision, secret, altered.toString("base64url")));
});
