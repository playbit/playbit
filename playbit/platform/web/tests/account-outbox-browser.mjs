import { numberedId } from './identifier-fixture.mjs';
import { encodeId, validId } from '../src/identifier.ts';
// Actual IndexedDB/WebCrypto. Run as a module worker; post {} for correctness or {benchmark:true,samples:1000}.
// Each run uses isolated generated account keys and removes only those rows.
import { personalSelections } from '../src/personal.ts';
import { accountCipher } from '../src/account-record.ts';
import { encodePersonalRecord, decodePersonalRecord } from '../src/account-format.ts';
import { openAccountMetadata, readAccountMutation, commitAccountMutation, adoptAccountMutation, pendingAccountRecord, acknowledgeAccountRecord, observeAccountMetadata, nextAccountClock, requireAccountAuthority, quarantineAccount } from '../src/account-outbox.ts';
import { RecoveryClient } from '../src/recovery.ts';
import { webAccountReplicaCases } from './account-replica-cases.mjs';
import { webAccountRecordCases } from './account-record-cases.mjs';
import { webStorageFormatCases } from './storage-format-browser.mjs';
const check = (value, message) => { if (!value) throw Error(message); };
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const hex = bytes => Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
const summary = values => {
    values = values.slice().sort((a, b) => a - b);
    return { n: values.length, p50_ms: values[Math.ceil(values.length * .5) - 1], p99_ms: values[Math.ceil(values.length * .99) - 1], max_ms: values.at(-1) };
};
self.onmessage = async ({ data }) => {
    self.onmessage = null;
    if (data.storageFormatCheck) {
        try { self.postMessage({ result: await webStorageFormatCases() }); }
        catch (error) { self.postMessage({ error: error.stack || error.message }); }
        self.postMessage({ cleaned: true }); self.close(); return;
    }
    if (data.quarantineCheck) {
        try { await requireAccountAuthority(data.quarantineCheck); self.postMessage({ quarantined: false }); }
        catch (error) { self.postMessage({ quarantined: true, code: error.code, message: error.message }); }
        self.close(); return;
    }
    if (data.quarantineWrite) {
        const { account, record } = data.quarantineWrite, request = navigator.locks.request.bind(navigator.locks);
        navigator.locks.request = (name, options, callback) => {
            const result = request(name, options, callback);
            if (name === `playbit-account-authority:${account}` && options.mode === 'exclusive') {
                // A following independent grant confirms the lock manager has
                // processed the exclusive request, without timing or polling.
                void request(`quarantine-probe:${account}`, () => self.postMessage({ quarantineQueued: true }));
            }
            return result;
        };
        try { await quarantineAccount(account, record); self.postMessage({ quarantined: true }); }
        catch (error) { self.postMessage({ error: error.stack || error.message }); }
        self.close(); return;
    }
    const benchmark = data.benchmark === true;
    const samples = benchmark ? data.samples : 2;
    const now = benchmark ? () => performance.now() : () => 0;
    const accounts = [encodeId(crypto.getRandomValues(new Uint8Array(16))), encodeId(crypto.getRandomValues(new Uint8Array(16)))];
    const [account, locked] = accounts, secret = new Uint8Array(16).fill(7), version = encodeId(new Uint8Array(32).fill(0xab)), volume = encodeId(new Uint8Array(16).fill(0xcd));
    let notices = 0;
    observeAccountMetadata(id => { check(accounts.includes(id), 'notice crossed account identity'); notices++; });
    const originalTransaction = IDBDatabase.prototype.transaction, originalPut = IDBObjectStore.prototype.put;
    const originalEncrypt = crypto.subtle.encrypt.bind(crypto.subtle);
    const timings = [], reads = [], commits = [], seals = [];
    let measuring = false;
    const write = async (kind, account, value) => {
        const { database, store } = await openAccountMetadata(kind);
        try { await new Promise((resolve, reject) => {
            const tx = database.transaction(store, 'readwrite', { durability: 'strict' });
            tx.objectStore(store).put(value, account); tx.oncomplete = resolve; tx.onabort = () => reject(tx.error);
        }); } finally { database.close(); }
    };
    try {
        check(Number.isInteger(samples) && samples > 0 && samples <= 1000, 'Use1–1000 samples');
        for (const id of accounts) await write('recovery', id, { catalog: { account: id, volumes: [], invitations: [] },
            secret: Array.from(secret), acknowledged: id === account, revision: id === account ? 1 : 0 });
        const versions = Object.fromEntries(Array.from({ length: 128 }, (_, index) => [numberedId(index, 32), [{ volume, write: false, incarnation: numberedId(index + 1) }]]));
        await write('personal', account, versions);
        check(same(await personalSelections(account), versions), 'existing consent rows changed');
        await personalSelections(locked, version, []);
        check(await pendingAccountRecord('personal', locked) === null, 'unsaved recovery published intent');
        const cipher = await accountCipher(account, secret);
        const expectedKey = hex(await cipher.recordKey(new TextEncoder().encode(`session-mounts/${version}`)));
        await personalSelections(account, version, []);
        const first = await pendingAccountRecord('personal', account);
        check(notices === 2, 'committed local decisions must notify, including locked state');
        check(first.key === expectedKey, 'native logical-key mismatch');
        const decoded = decodePersonalRecord(await cipher.open(Uint8Array.from(first.key.match(/../g), byte => parseInt(byte, 16)), first.clock, first.ciphertext));
        check(decoded.session === version && same(decoded.grants, []), 'native typed-record mismatch');
        await personalSelections(account, version, []);
        check(notices === 2, 'unchanged decision emitted a notice');
        const replay = await pendingAccountRecord('personal', account);
        check(replay.clock === first.clock && same(Array.from(replay.ciphertext), Array.from(first.ciphertext)), 'unchanged decision resealed');
        await personalSelections(account, version, [{ volume, write: true }], []);
        check(!await acknowledgeAccountRecord('personal', account, first), 'old acknowledgment cleared newer decision');
        check(notices === 3, 'stale acknowledgment emitted a notice');
        const originalGrant = (await personalSelections(account))[version][0].incarnation;
        check(validId(originalGrant) && originalGrant !== '1'.repeat(16), 'grant incarnation is missing');
        const current = await pendingAccountRecord('personal', account);
        check(current.clock > first.clock, 'clock did not advance');
        check(!await acknowledgeAccountRecord('personal', account, current, () => false), 'retired publication acknowledged intent');
        check((await pendingAccountRecord('personal', account)).clock === current.clock, 'retired acknowledgment cleared intent');
        check(await acknowledgeAccountRecord('personal', account, current), 'exact acknowledgment failed');
        check(!await acknowledgeAccountRecord('personal', account, current), 'duplicate acknowledgment changed state');
        check(notices === 4, 'exact acknowledgment must notify once');
        check(await pendingAccountRecord('personal', account) === null, 'acknowledgment did not clear marker');
        await personalSelections(account, version, [{ volume, write: true }]);
        check(await pendingAccountRecord('personal', account) === null, 'unchanged acknowledged decision became dirty');
        check((await personalSelections(account))[version][0].incarnation === originalGrant, 'unchanged selection replaced grant identity');
        const before = await personalSelections(account);
        IDBObjectStore.prototype.put = function(value, key) {
            const result = originalPut.call(this, value, key);
            if (Array.isArray(key) && key[0] === account && key[1] === 'record') this.transaction.abort();
            return result;
        };
        let aborted = false;
        try { await personalSelections(account, version, []); } catch { aborted = true; }
        IDBObjectStore.prototype.put = originalPut;
        check(aborted && same(await personalSelections(account), before), 'aborted transaction changed consent');
        check(notices === 4, 'aborted transaction emitted a committed notice');
        check(await pendingAccountRecord('personal', account) === null, 'aborted transaction retained outbox prefix');
        const metadata = await openAccountMetadata('personal');
        try {
            const stale = await readAccountMutation(metadata, account, first.key);
            await personalSelections(account, version, []);
            let refused = false;
            try { await commitAccountMutation(metadata, account, stale.control, stale.state); } catch { refused = true; }
            check(refused && same((await personalSelections(account))[version], []), 'stale preparation overwrote denial');
        } finally { metadata.database.close(); }
        IDBDatabase.prototype.transaction = function(...args) {
            const measured = measuring, started = now(), tx = originalTransaction.apply(this, args);
            if (measured) tx.addEventListener('complete', () => (tx.mode === 'readwrite' ? commits : reads).push(now() - started), { once: true });
            return tx;
        };
        crypto.subtle.encrypt = async (...args) => {
            const measured = measuring, started = now();
            try { return await originalEncrypt(...args); } finally { if (measured) seals.push(now() - started); }
        };
        for (let index = 0; index < samples; index++) {
            const grants = index % 2 ? [] : [{ volume, write: false }];
            const start = now(); measuring = benchmark;
            await personalSelections(account, version, grants);
            measuring = false; if (benchmark) timings.push(now() - start);
        }
        const retained = await pendingAccountRecord('personal', account);
        check(retained && await pendingAccountRecord('personal', account, retained.key) === null, 'mutations did not coalesce to one record');
        const after = await personalSelections(account);
        check(Object.entries(versions).every(([key, value]) => same(after[key], value)), 'older versions changed');
        check(Object.keys(after).length === 129, 'outbox metadata escaped into version map');
        check(await pendingAccountRecord('recovery', account) === null, 'outbox crossed metadata store');
        const adoptionTimings = [];
        const adoptionMetadata = await openAccountMetadata('personal');
        try {
            const snapshot = await readAccountMutation(adoptionMetadata, account, retained.key);
            const unchangedNotices = notices;
            check(!await adoptAccountMutation(adoptionMetadata, account, snapshot.control, {}, retained), 'exact local replay changed projection');
            check(notices === unchangedNotices && await pendingAccountRecord('personal', account), 'exact replay cleared local dirty intent or notified');
            const key = Uint8Array.from(retained.key.match(/../g), byte => parseInt(byte, 16));
            const remote = async (clock, grants) => ({ key: retained.key, clock,
                ciphertext: await cipher.seal(key, clock, encodePersonalRecord(version, grants)) });
            const stale = await readAccountMutation(adoptionMetadata, account, retained.key);
            await personalSelections(account, version, [{ volume, write: true }]);
            check((await personalSelections(account))[version][0].incarnation !== originalGrant, 'withdrawal/regrant reused a capability incarnation');
            const pending = await pendingAccountRecord('personal', account);
            const future = await remote(pending.clock + 0x100000000n, []);
            let refused = false;
            try { await adoptAccountMutation(adoptionMetadata, account, stale.control, {}, future); }
            catch (error) { refused = error.code === 'account-metadata-changed'; }
            check(refused && (await pendingAccountRecord('personal', account)).clock === pending.clock, 'stale projection replaced newer intent');
            const before = await readAccountMutation(adoptionMetadata, account, retained.key);
            check(!await adoptAccountMutation(adoptionMetadata, account, before.control, {}, future, () => false), 'retired receipt changed state');
            check((await pendingAccountRecord('personal', account)).clock === pending.clock, 'retired receipt cleared intent');
            IDBObjectStore.prototype.put = function(value, key) {
                const result = originalPut.call(this, value, key);
                if (Array.isArray(key) && key[0] === account && key[1] === 'record') this.transaction.abort();
                return result;
            };
            let aborted = false;
            try { await adoptAccountMutation(adoptionMetadata, account, before.control, { ...before.state, [version]: [] }, future); }
            catch { aborted = true; }
            IDBObjectStore.prototype.put = originalPut;
            const untouched = await readAccountMutation(adoptionMetadata, account, retained.key);
            check(aborted && same(untouched.state, before.state) && untouched.control.revision === before.control.revision
                && untouched.record.clock === pending.clock && (await pendingAccountRecord('personal', account)).clock === pending.clock, 'aborted adoption committed a prefix');
            const equalClock = [future, await remote(future.clock, [{ volume, write: false, incarnation: encodeId(new Uint8Array(16).fill(0x41)) }])];
            equalClock.sort((a, b) => {
                for (let i = 0; i < Math.min(a.ciphertext.length, b.ciphertext.length); i++) {
                    if (a.ciphertext[i] !== b.ciphertext[i]) return a.ciphertext[i] - b.ciphertext[i];
                }
                return a.ciphertext.length - b.ciphertext.length;
            });
            for (const incoming of equalClock) {
                const current = await readAccountMutation(adoptionMetadata, account, retained.key);
                const plain = decodePersonalRecord(await cipher.open(key, incoming.clock, incoming.ciphertext));
                check(await adoptAccountMutation(adoptionMetadata, account, current.control, { ...current.state, [version]: plain.grants }, incoming), 'valid clock/ciphertext winner not adopted');
            }
            check(await pendingAccountRecord('personal', account) === null, 'remote winner retained obsolete dirty intent');
            check(!await adoptAccountMutation(adoptionMetadata, account, stale.control, {}, equalClock[0]), 'older tied value overwrote its winner');
            for (let index = 0; index < samples; index++) {
                const current = await readAccountMutation(adoptionMetadata, account, retained.key), grants = index % 2 ? [] : [{ volume, write: false, incarnation: numberedId(index + 1) }];
                const incoming = await remote(nextAccountClock(current.control.clock), grants);
                const start = now();
                check(await adoptAccountMutation(adoptionMetadata, account, current.control, { ...current.state, [version]: grants }, incoming), 'remote transition did not commit');
                if (benchmark) adoptionTimings.push(now() - start);
            }
            const observed = await readAccountMutation(adoptionMetadata, account, retained.key);
            await personalSelections(account, version, [{ volume, write: true }]);
            check((await pendingAccountRecord('personal', account)).clock > observed.control.clock, 'local clock did not advance past remote receipt');
        } finally { adoptionMetadata.database.close(); }
        // Exercise the production recovery store, not the in-memory protocol
        // adapter. A prepared recovery write must notice another local writer.
        const recovery = new RecoveryClient({ request() { throw Error('Unexpected AP request'); } }, account);
        const staleRecovery = await recovery.snapshot(), freshRecovery = await recovery.snapshot();
        freshRecovery.state.catalog.volumes.push({ volume: { id: volume, owner: account, name: 'Concurrent recovery', share_key_with_service: false, personal: true },
            credentials: { database_key: Array(32).fill(3), blob_key: Array(32).fill(4) } });
        await recovery.store(freshRecovery);
        let staleRecoveryRefused = false;
        try { await recovery.store(staleRecovery); } catch (error) { staleRecoveryRefused = error.code === 'account-metadata-changed'; }
        const recoveryAfter = await new RecoveryClient({}, account).state();
        check(staleRecoveryRefused && recoveryAfter.catalog.volumes.length === 1 && recoveryAfter.revision === 1
            && recoveryAfter.acknowledged && same(recoveryAfter.secret, Array.from(secret)), 'stale recovery lost credentials or acknowledgment');
        const recoveryFixture = await recovery.snapshot();
        for (let index = 1; index < 128; index++) recoveryFixture.state.catalog.volumes.push({
            volume: { id: numberedId(index), owner: account, name: `Retained ${index}`, share_key_with_service: false, personal: true },
            credentials: { database_key: Array(32).fill(3), blob_key: Array(32).fill(4) },
        });
        await recovery.store(recoveryFixture);
        const recoveryTimings = [];
        for (let index = 0; index < samples; index++) {
            const start = now(), snapshot = await recovery.snapshot();
            snapshot.state.catalog.volumes[0].volume.name = index % 2 ? 'Even' : 'Odd';
            await recovery.store(snapshot);
            if (benchmark) recoveryTimings.push(now() - start);
        }
        const typed_records = await webAccountRecordCases({ benchmark });
        const account_replica = await webAccountReplicaCases(accounts, { benchmark });
        self.postMessage({ result: { typed_records, account_replica, checks: '128 existing versions retained; exact denial/version binding; native record encoding; stable replay; stale ACK; abort atomicity; stale preparation; one coalesced record; unacknowledged recovery local-only; commit-only notices and exact ACK notice',
            boundary: 'personalSelections through strict IndexedDB commit, remote adoption and recovery; excludes AP/UI',
            userAgent: navigator.userAgent, crossOriginIsolated,
            ...(benchmark ? { samples, total: summary(timings),
                readonly_transactions: summary(reads), strict_commits: summary(commits), seal: summary(seals),
                adoption_strict_commit: summary(adoptionTimings), recovery_read_and_strict_commit: summary(recoveryTimings) } : {}),
            adoption_checks: 'exact local replay stays dirty; stale projection rejected; abort preserves state/head/clock/dirty; clock/ciphertext ties; observed clock; production recovery CAS and reopen',
            recovery_retained_volumes: 128,
            retained_versions: Object.keys(after).length, pending_records: 1, record_bytes: retained.ciphertext.length + 40,
            ...(benchmark ? { timing_caution: 'Non-isolated timers quantize short spans. No native/UI/AP qualification.' } : {}) } });
    } catch (error) { self.postMessage({ error: error.stack || error.message }); }
    finally {
        measuring = false; IDBDatabase.prototype.transaction = originalTransaction; IDBObjectStore.prototype.put = originalPut; crypto.subtle.encrypt = originalEncrypt;
        for (const kind of ['personal', 'recovery']) {
            const { database, store } = await openAccountMetadata(kind);
            try { await new Promise((resolve, reject) => {
                const tx = database.transaction(store, 'readwrite', { durability: 'strict' }), entries = tx.objectStore(store);
                for (const account of accounts) { entries.delete(account); entries.delete(IDBKeyRange.bound([account], [account, []])); }
                tx.oncomplete = resolve; tx.onabort = () => reject(tx.error);
            }); } finally { database.close(); }
        }
        self.postMessage({ cleaned: true });
        self.close();
    }
};
