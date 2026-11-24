import { decodeId, encodeId, validId } from './identifier.ts';
import { openMetadata, watchMetadata, requireStorageFormat } from './storage-format.ts';
import { configuration } from './release-channel.ts';
import type { Deployment } from './release-channel.ts';
import { authenticationReply, validateApTiming } from "./ap-timing.ts";
import { parseAccountKey, accountSigningKey, loginMessage } from "./account-key.ts";
import { WebApTransport } from "./ap-transport.ts";
import type { ApTransport } from "./ap-transport.ts";
import { failure, transportFailure } from "./errors.ts";
import { AP, SERVICE_PROTOCOL, MAX_SERVICE_REQUEST, MAX_SERVICE_DESCRIPTION, FrameReader, encodeFrame, registrationFrame, registrationResponse, certificateRequest, text, utf8 } from "./ap-codec.ts";
import type { FrameInput, Frame } from "./ap-codec.ts";
import { MAX_REPLICA_EVENT, replicaInteger, snapshotInteger, bloblogInteger, subscriptionAck, replicaEvent } from "./replication.ts";
import type { ApMethods, ApOperation, ApResponse, ApDescription, ApStatus, ApReplicaSubscription, ApPublicationAck, ApInvitationEvent } from "./ap-protocol.ts";
import { advisoryOperation, type AdvisoryOperation } from './ap-protocol.ts';

// Web upload sources retain their 64 MiB admission ceiling. Package
// downloads use bounded streaming in package-stream.ts.
const MAX_UPLOAD_BYTES = 64 * 1024 * 1024;
function admittedWrite(writer: WritableStreamDefaultWriter<Uint8Array>, bytes: Uint8Array): Promise<void> {
    requireStorageFormat();
    return writer.write(bytes);
}

// The web browser's AP enrollment, stored under the "active" credential key. The
// registration fields are absent until `finishCode` completes.
export interface ApIdentity {
    email: string; instanceName: string; keyPair: CryptoKeyPair;
    onboardingSeen?: number; onboardingAcknowledged?: number;
    authToken?: Uint8Array; instanceId?: Uint8Array; caCertificate?: Uint8Array; instanceCertificate?: Uint8Array;
}
// An enrollment that AP has registered: what `ApConnection.identity` holds once ready.
export type ApRegisteredIdentity = ApIdentity & Required<Pick<ApIdentity, "authToken" | "instanceId" | "caCertificate" | "instanceCertificate">>;
// Reads (`value` absent) resolve with the stored identity, if any; the resolved
// value of a write is ignored.
export type ApCredentialStore = (account: string, value?: ApIdentity) => Promise<ApIdentity | undefined>;
export type ApTransportFactory = () => ApTransport;
export type ApConnectionStatus = ApStatus;

// What a channel listener receives: a published frame, or the channel's terminal failure.
export type ApChannelMessage =
    | { channel: string; source: Uint8Array; body: Uint8Array; error?: undefined }
    | { channel: string; error: string; source?: undefined; body?: undefined };
export type ApChannelReceiver = (message: ApChannelMessage) => void;
export interface ApChannel { writer: WritableStreamDefaultWriter<Uint8Array>; reader: FrameReader; listeners: Set<ApChannelReceiver> }

// One operation stream. `writer` and `reader` are set by the first step of
// `begin`; every later step runs on a stream that has them.
interface StreamState {
    operation: ApOperation; busy: boolean; closed: boolean; finished: boolean; handle: number;
    writer?: WritableStreamDefaultWriter<Uint8Array>; reader?: FrameReader;
    timer?: ReturnType<typeof setTimeout>; replica?: boolean; channel?: Uint8Array; controlWriting?: boolean; advisory?: AdvisoryOperation;
    upload?: ReadableStreamDefaultReader<Uint8Array> | null;
}

async function credentials(account: string, value?: ApIdentity): Promise<ApIdentity | undefined> {
    await watchMetadata('playbit-ap', 'credentials', 'playbit.web.ap-credentials');
    const database = await openMetadata('playbit-ap', 'credentials', 'playbit.web.ap-credentials');
    try {
        return await new Promise<ApIdentity | undefined>((resolve, reject) => {
            const transaction = database.transaction("credentials", value ? "readwrite" : "readonly");
            const store = transaction.objectStore("credentials");
            const request = value ? store.put(value, account) : store.get(account);
            transaction.oncomplete = () => resolve(request.result);
            transaction.onabort = () => reject(transaction.error ?? new Error("Credential transaction aborted"));
            transaction.onerror = () => reject(transaction.error);
        });
    } finally { database.close(); }
}

function deadline<T>(promise: Promise<T>, milliseconds: number, cancel: () => void): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    // The abandoned work usually rejects after the race has settled; keep that
    // from surfacing as an unhandled rejection.
    promise.catch(() => {});
    return Promise.race([promise, new Promise<never>((_, reject) => {
        timer = setTimeout(() => {
            cancel();
            reject(failure("ap-transport", "AP operation timed out"));
        }, milliseconds);
    })]).finally(() => clearTimeout(timer));
}

// One instance belongs to the account's SharedWorker, never to an app/tab.
export class ApConnection {
    private channelTicket = 0;
    private channelBytes = 0;
    private readonly advisory = new Set<AdvisoryOperation>();
    private sessionDisplayNames?: boolean;
    private readonly channelSessions = new Map<string, number>();
    private readonly channelDeliveries = new Map<number, {session:string;bytes:number}>();
    onboardingTask: Promise<void> | null = null;
    onboardingDirty = false;
    credentialStore: ApCredentialStore;
    transportFactory: ApTransportFactory;
    streams: Map<number, StreamState>;
    nextStream: number;
    authenticating: boolean;
    account: string | null;
    transport: ApTransport | null;
    ready: Promise<void> | null;
    identity: ApRegisteredIdentity | null;
    active: number;
    replicaStreams: number;
    connectionCount: number;
    channels: Map<string, Promise<ApChannel>>;
    operationTimeout: number;
    description: Promise<ApDescription> | null;
    // Set by the first `connect`; the constructor creates no such property.
    declare connecting: Promise<ApStatus> | null | undefined;

    constructor(selected: Deployment, { credentialStore = credentials, transportFactory = () => new WebApTransport(`https://${configuration(selected).ap_hostname}/`) }: { credentialStore?: ApCredentialStore; transportFactory?: ApTransportFactory } = {}) {
        configuration(selected);
        this.credentialStore = credentialStore;
        this.transportFactory = transportFactory;
        this.streams = new Map();
        this.nextStream = 1;
        this.authenticating = false;
        this.account = null;
        this.transport = null;
        this.ready = null;
        this.identity = null;
        this.active = 0;
        this.replicaStreams = 0;
        this.connectionCount = 0;
        this.channels = new Map();
        this.description = null;
        // How long one operation may take before its stream is abandoned.
        this.operationTimeout = 60000;
    }

    async transportReady(): Promise<ApTransport> {
        requireStorageFormat();
        if (!this.transport) {
            const transport = this.transportFactory();
            this.transport = transport;
            this.connectionCount++;
            this.description = null;
            this.sessionDisplayNames = undefined;
            transport.closed.then(() => this.disconnected(transport), () => this.disconnected(transport));
        }
        const transport = this.transport;
        await transport.ready.catch(error => { throw transportFailure(error); });
        requireStorageFormat();
        if (this.transport !== transport) throw failure('ap-transport', 'AP connection closed during setup');
        return transport;
    }

    async authenticationFrame(frame: FrameInput): Promise<Frame> {
        const transport = await this.transportReady();
        const stream = await transport.createBidirectionalStream().catch(error => { throw transportFailure(error); });
        const writer = stream.writable.getWriter(), reader = new FrameReader(stream.readable);
        try {
            await admittedWrite(writer, encodeFrame(frame)).catch(error => { throw transportFailure(error); });
            // An unauthenticated AP stream may accept another registration
            // frame. Closing it early lets AP drop the connection before its
            // response reaches the web browser.
            return authenticationReply(await reader.frame(4096));
        } finally {
            // AP may close the unauthenticated connection as soon as it sees
            // EOF. Cleanup must not replace an already received response.
            await writer.close().catch(() => {});
            writer.releaseLock();
            await reader.close().catch(() => {});
        }
    }

    async authenticate<T>(work: () => Promise<T>): Promise<T> {
        if (this.authenticating) throw new Error("Sign-in is already in progress");
        this.authenticating = true;
        try {
            return await deadline(work(), 20000, () => this.transport?.close());
        } catch (error) {
            this.transport?.close();
            this.transport = null;
            this.ready = null;
            throw error;
        } finally { this.authenticating = false; }
    }

    async onboarding(stage: number): Promise<void> {
        if (!Number.isInteger(stage) || stage < 4 || stage > 7) throw Error("Invalid onboarding milestone");
        const identity = this.identity;
        if (!identity || !this.ready) throw Error("Onboarding requires authentication");
        identity.onboardingSeen = (identity.onboardingSeen ?? 0) | (1 << (stage - 4));
        await this.credentialStore("active", identity);
        this.onboardingDirty = true;
        if (this.onboardingTask) return;
        this.onboardingTask = (async () => {
            do {
                this.onboardingDirty = false;
                try { if (this.identity) await this.flushOnboarding(this.identity); } catch {
                    // A reconnect or new milestone during a failed send must still drain.
                }
            } while (this.onboardingDirty);
        })().finally(() => { this.onboardingTask = null; });
    }
    async flushOnboarding(identity: ApRegisteredIdentity): Promise<void> {
        for (let stage = 4; stage <= 7; stage++) {
            const bit = 1 << (stage - 4);
            if (!((identity.onboardingSeen ?? 0) & bit) || ((identity.onboardingAcknowledged ?? 0) & bit)) continue;
            if (this.identity !== identity || !this.transport) return;
            const reply = await this.onboardingFrame({ type: AP.ONBOARDING, body: Uint8Array.of(stage) });
            if (reply.type !== AP.ONBOARDING || reply.flags || reply.source.length || reply.destination.length || reply.body.length !== 1 || reply.body[0] !== stage) throw Error("Invalid onboarding acknowledgment");
            if (this.identity !== identity) return;
            identity.onboardingAcknowledged = (identity.onboardingAcknowledged ?? 0) | bit;
            await this.credentialStore("active", identity);
        }
    }

    async agreement({ account, revision, sha256, accept }: ApMethods["agreement"]["args"]): Promise<boolean> {
        if (!this.identity || !account || this.account !== account) throw Error("Agreement requires the signed-in account");
        if (!Number.isInteger(revision) || revision < 1 || revision > 255 || typeof accept !== "boolean") throw Error("Invalid agreement request");
        const digest = decodeId(sha256, 32);
        if (!this.ready) {
            await this.connect({ account });
            if (!this.ready || this.account !== account) throw Error("The signed-in account changed while checking the agreement");
        }
        const reply = await this.onboardingFrame({ type: AP.AGREEMENT, flags: accept ? revision : 0, body: digest });
        if (!this.ready || this.account !== account) throw Error("The signed-in account changed while checking the agreement");
        if (reply.type !== AP.AGREEMENT || reply.source.length || reply.destination.length || !equalBytes(reply.body, digest)
            || (reply.flags !== revision && (accept || reply.flags !== 0))) throw Error("Playbit could not verify this agreement. Check for an update and try again.");
        return reply.flags === revision;
    }

    private async onboardingFrame(request: FrameInput): Promise<Frame> {
        const transport = this.transport;
        if (!transport) throw failure("ap-transport", "AP is disconnected");
        let expired = false;
        let reader: FrameReader | undefined, writer: WritableStreamDefaultWriter<Uint8Array> | undefined;
        return deadline((async () => {
            const stream = await transport.createBidirectionalStream();
            if (expired || this.transport !== transport) {
                void stream.readable.cancel().catch(() => {});
                void stream.writable.abort().catch(() => {});
                throw failure("ap-transport", "The account connection changed");
            }
            writer = stream.writable.getWriter();
            reader = new FrameReader(stream.readable);
            try {
                await admittedWrite(writer, encodeFrame(request));
                await writer.close().catch(() => {});
                return await reader.frame(32);
            } finally { writer.releaseLock(); void reader.close().catch(() => {}); }
        })(), 10000, () => { expired = true; void reader?.close().catch(() => {}); void writer?.abort().catch(() => {}); });
    }

    async requestCode({ email, instanceName = "Web" }: { email: string; instanceName?: string }): Promise<{ email: string; codeSent: true }> {
        email = String(email).trim().toLowerCase();
        if (!email.includes("@") || email.length > 254 || /\s/.test(email)) throw new Error("Enter a valid email address");
        if (this.ready) throw new Error("This web browser is already signed in");
        return this.authenticate(async () => {
            let identity = await this.credentialStore("active");
            if (!identity || identity.email !== email || identity.authToken) {
                this.account = null; this.identity = null;
                identity = { email, instanceName,
                    keyPair: await crypto.subtle.generateKey("Ed25519", false, ["sign", "verify"]) };
            }
            // The authentication key survives an email delay, reload, or lost response.
            await this.credentialStore("active", identity);
            const response = registrationResponse(await this.authenticationFrame(registrationFrame(email, identity.instanceName, "")));
            if (response.status !== 0) throw new Error(response.status === 3 ? "Account access is disabled"
                : response.status === 4 ? "Email delivery hasn’t been confirmed. Check your inbox or try sending again in a minute."
                : "Could not send a sign-in code");
            // AP authenticates its first stream before accepting operation streams.
            // Email entry can take minutes, so finish on a fresh connection rather
            // than retaining an unauthenticated stream beyond AP's auth deadline.
            const transport = this.transport;
            this.disconnected(transport);
            transport?.close();
            return { email, codeSent: true };
        });
    }

    async finishCode({ code, account = null }: { code: string; account?: string | null }): Promise<ApStatus> {
        if (typeof code !== "string" || code.length !== 6 || !/^[0-9]{6}$/.test(code)) throw new Error("Enter the six-digit email sign-in code");
        return this.authenticate(async () => {
            const identity = await this.credentialStore("active");
            if (!identity) throw new Error("Request a sign-in code first");
            const csr = await certificateRequest(identity.keyPair);
            const response = registrationResponse(await this.authenticationFrame(
                registrationFrame(identity.email, identity.instanceName, code, csr)));
            if (response.status !== 1) throw new Error(response.status === 2 ? "Invalid or expired sign-in code" : "Account sign-in is unavailable");
            if (!response.instanceId.length) throw new Error("Invalid AP registration identity");
            this.bindAccount(response.authToken, account);
            Object.assign(identity, response);
            // `status` came along with the registration fields just merged in.
            delete (identity as { status?: number }).status;
            await this.credentialStore("active", identity);
            // The merge above added every registration field.
            this.identity = identity as ApRegisteredIdentity;
            this.ready = Promise.resolve();
            void this.onboarding(4).catch(() => {});
            return this.status();
        });
    }

    async signInWithKey({ email, accountKey }: { email: string; accountKey: string }): Promise<ApStatus> {
        email = email.trim().toLowerCase();
        if (!email.includes("@") || email.length > 254 || /\s/.test(email)) throw Error("Enter a valid email address");
        const seed = parseAccountKey(accountKey);
        if (this.ready) throw Error("This web browser is already signed in");
        return this.authenticate(async () => {
            let identity = await this.credentialStore("active");
            if (!identity || identity.email !== email || identity.authToken) {
                this.account = null; this.identity = null;
                identity = { email, instanceName: "Web", keyPair: await crypto.subtle.generateKey("Ed25519", false, ["sign", "verify"]) };
            }
            await this.credentialStore("active", identity);
            const request = registrationFrame(email, identity.instanceName, "", await certificateRequest(identity.keyPair));
            request.type = AP.KEY_CHALLENGE;
            request.flags = 0;
            const transport = await this.transportReady();
            const stream = await transport.createBidirectionalStream();
            const writer = stream.writable.getWriter(), reader = new FrameReader(stream.readable);
            try {
                await admittedWrite(writer, encodeFrame(request));
                const challenge = authenticationReply(await reader.frame(4096));
                if (challenge.type !== AP.KEY_CHALLENGE || challenge.flags !== 0 || challenge.body.length !== 32 || challenge.source.length || challenge.destination.length) throw Error("Invalid account key challenge");
                const { privateKey } = await accountSigningKey(seed);
                const proof = new Uint8Array(await crypto.subtle.sign("Ed25519", privateKey, loginMessage(challenge.body, request.body!)));
                await admittedWrite(writer, encodeFrame({ type: AP.KEY_PROOF, body: proof }));
                const response = registrationResponse(authenticationReply(await reader.frame(4096)));
                if (response.status !== 1) throw Error("Email or account key is incorrect");
                this.bindAccount(response.authToken);
                Object.assign(identity, response);
                delete (identity as { status?: number }).status;
                await this.credentialStore("active", identity);
                this.identity = identity as ApRegisteredIdentity;
                this.ready = Promise.resolve();
                void this.onboarding(4).catch(() => {});
                return this.status();
            } finally {
                await writer.close().catch(() => {}); writer.releaseLock(); await reader.close().catch(() => {});
            }
        });
    }

    bindAccount(token: Uint8Array, expected?: string | null): void {
        if (!(token instanceof Uint8Array) || token.length !== 72) throw new Error("Invalid AP authentication token");
        const account = encodeId(token.subarray(0, 16));
        if ((expected && account !== expected) || (this.account && this.account !== account)) {
            throw new Error("The signed-in account differs from the requested account");
        }
        this.account = account;
    }

    async connect({ account = null }: { account?: string | null } = {}): Promise<ApStatus> {
        requireStorageFormat();
        if (account !== null && !validId(account, 16)) throw new Error("Invalid AP account identity");
        if (this.ready) {
            await this.ready;
            if (account && this.account !== account) throw new Error("AP worker belongs to another account");
            return this.status();
        }
        if (this.connecting) {
            await this.connecting;
            if (account && this.account !== account) throw new Error("AP worker belongs to another account");
            return this.status();
        }
        this.connecting = this.authenticate(async () => {
            const identity = await this.credentialStore("active");
            if (!identity?.authToken?.length) throw failure("authentication-required", "Sign in with your email address to continue");
            this.bindAccount(identity.authToken, account);
            const response = await this.authenticationFrame({ type: AP.HELLO, body: identity.authToken });
            if (response.type !== AP.HELLO || response.flags !== 0) throw failure("authentication-required", "Sign-in expired. Request another email code");
            if (response.body.length) identity.authToken = response.body;
            this.bindAccount(identity.authToken, account);
            await this.credentialStore("active", identity);
            // `finishCode` stores the token together with the other registration fields.
            this.identity = identity as ApRegisteredIdentity;
            this.ready = Promise.resolve();
            void this.onboarding(4).catch(() => {});
            return this.status();
        });
        try { return await this.connecting; } finally { this.connecting = null; }
    }

    disconnected(transport: ApTransport | null): void {
        if (this.transport === transport) {
            this.transport = null;
            this.ready = null;
            this.channels.clear();
            this.description = null;
            this.sessionDisplayNames = undefined;
            for (const handle of this.streams.keys()) this.streamClose(handle);
        }
    }

    async channel(name: string): Promise<ApChannel> {
        if (!validId(name, 32)) throw new Error("Invalid AP channel identity");
        if (!this.ready) throw failure("ap-transport", "AP is disconnected");
        await this.ready;
        const transport = this.transport;
        if (!transport) throw failure("ap-transport", "AP is disconnected");
        if (!this.channels.has(name)) {
            if (this.channels.size >= 16) throw new Error("Too many AP channels");
            // `| undefined` only for the type checker: the reader loop below is
            // an immediately invoked function, so TS analyzes its reference to
            // `pending` as if it ran before this assignment.
            const pending: Promise<ApChannel> | undefined = (async () => {
                const stream = await transport.createBidirectionalStream().catch(error => { throw transportFailure(error); });
                const writer = stream.writable.getWriter(), reader = new FrameReader(stream.readable);
                const listeners = new Set<ApChannelReceiver>();
                await admittedWrite(writer, encodeFrame({ type: AP.SUBSCRIBE, destination: utf8.encode(name) }));
                const ack = await reader.frame();
                if (ack.type !== AP.SUBSCRIBE || text.decode(ack.destination) !== name) throw new Error("AP subscription rejected");
                const channel = { writer, reader, listeners };
                (async () => {
                    try {
                        for (;;) {
                            const frame = await reader.frame();
                            if (frame.type !== AP.PUBLISH || text.decode(frame.destination) !== name) throw new Error("Invalid AP channel frame");
                            for (const listener of listeners) listener({ channel: name, source: frame.source, body: frame.body });
                        }
                    } catch (error) {
                        if (this.channels.get(name) === pending) this.channels.delete(name);
                        // The failure is whatever the stream threw; read `message` off any value.
                        for (const listener of listeners) listener({ channel: name, error: String((error as { message?: unknown } | null)?.message ?? error) });
                    } finally {
                        // `detach` may already have ended this stream, so neither
                        // of these has to still be ours to release.
                        await reader.close().catch(() => {});
                        try { writer.releaseLock(); } catch { /* already released */ }
                    }
                })();
                return channel;
            })().catch(error => { if (this.channels.get(name) === pending) this.channels.delete(name); throw error; });
            this.channels.set(name, pending);
        }
        // Present: either found above or set synchronously just now.
        return this.channels.get(name)!;
    }

    async subscribe(name: string, listener: ApChannelReceiver): Promise<{ subscribed: string }> {
        (await this.channel(name)).listeners.add(listener);
        return { subscribed: name };
    }

    async publish(name: string, bytes: Uint8Array): Promise<{ sent: number }> {
        const channel = await this.channel(name);
        await admittedWrite(channel.writer, encodeFrame({ type: AP.PUBLISH, destination: utf8.encode(name), body: bytes }));
        return { sent: bytes.length };
    }

    detach(listener: ApChannelReceiver): void {
        for (const [name, pending] of this.channels) {
            pending.then(channel => {
                if (!channel.listeners.delete(listener) || channel.listeners.size) return;
                // Nobody in any tab is listening. A connection may hold only
                // sixteen channels and this worker outlives every tab, so give
                // the subscription up instead of holding it for a reader that
                // no longer exists.
                if (this.channels.get(name) === pending) this.channels.delete(name);
                channel.writer.abort().catch(() => {});
                channel.reader.close().catch(() => {});
            }).catch(() => {});
        }
    }

    status(): ApStatus {
        return { connected: this.ready !== null && this.identity !== null && this.transport !== null, account: this.account,
            // Enrollment retains canonical base58 text for AP authentication;
            // session-channel sources use the decoded fixed-width identity.
            caCertificate: this.identity?.caCertificate ? Array.from(this.identity.caCertificate) : undefined,
            instanceId: this.identity ? Array.from(decodeId(text.decode(this.identity.instanceId), 16)) : null,
            connectionCount: this.connectionCount, activeStreams: this.active,
            ...(this.sessionDisplayNames !== undefined && { sessionDisplayNames: this.sessionDisplayNames }) };
    }

    async beginChannel(frame: FrameInput,device:Uint8Array,generation:number): Promise<number> {
        if (!this.ready || !this.transport) throw failure("ap-transport", "AP is disconnected");
        if (!frame.destination || frame.destination.length<32 || frame.destination.length>255 || frame.flags!==1 || (frame.type!==AP.SUBSCRIBE&&frame.type!==AP.PUBLISH)) throw new Error("Invalid session channel");
        await this.ready;
        if (!(device instanceof Uint8Array) || device.length!==16 || !this.identity || encodeId(device)!==text.decode(this.identity.instanceId) || generation!==this.connectionCount)
            throw failure('ap-transport','The channel authentication connection changed');
        const transport=this.transport;
        if(!transport)throw failure('ap-transport','AP is disconnected');
        if (this.streams.size >= 1024) throw new Error("AP channel capacity exceeded");
        const handle=this.nextStream++;
        const state: StreamState={operation:{op:"session_channel",operation_id:encodeId(crypto.getRandomValues(new Uint8Array(16)))},busy:false,closed:false,finished:false,handle,channel:frame.destination.slice()};
        this.streams.set(handle,state);
        try {
            await deadline(this.streamStep(handle,async()=>{
                const stream=await transport.createBidirectionalStream();
                state.writer=stream.writable.getWriter(); state.reader=new FrameReader(stream.readable);
                if(state.closed||transport!==this.transport||generation!==this.connectionCount){this.abandonStream(state);throw failure('ap-transport','AP channel was retired before admission');}
                await admittedWrite(state.writer, encodeFrame(frame));
                const ack=await state.reader.frame(0);
                if (ack.type!==frame.type || ack.source.length || ack.body.length || !equalBytes(ack.destination,frame.destination!)) throw new Error("Invalid channel acknowledgement");
            }), this.operationTimeout, () => this.streamClose(handle));
            return handle;
        } catch(error) {this.streamClose(handle);throw error;}
    }
    channelRead(handle:number):Promise<Frame & {receipt?:number}> {
        return this.streamStep(handle,async state=>{
            if (!state.channel) throw new Error("Expected session channel stream");
            const frame = await state.reader!.frame(state.channel.length === 32 ? 64 * 1024 : 32 * 1024 + 44);
            return this.retainChannel(frame);
        });
    }
    async channelDisplayName(handle: number, name: Uint8Array): Promise<void> {
        const state = this.streams.get(handle);
        if (state?.channel?.length !== 32 || state.closed) throw failure("ap-transport", "Session membership stream is closed");
        if (!(name instanceof Uint8Array) || name.length > 256) throw Error('Invalid display name length');
        await this.writeControl(state, { type: AP.SUBSCRIBE, flags: 1, destination: state.channel, body: name });
    }
    retainChannel(frame:Frame):Frame & {receipt?:number} {
        if (frame.type!==AP.PUBLISH || frame.source.length!==80) return frame;
        const session=encodeId(frame.destination.slice(0,32));
        const bytes=frame.body.length+frame.source.length+frame.destination.length+32;
        const previous=this.channelSessions.get(session)??0;
        if (previous+bytes>65536 || this.channelBytes+bytes>262144 || this.channelDeliveries.size>=1024 || this.channelTicket>=Number.MAX_SAFE_INTEGER)
            throw failure('channel-overflow','Channel consumer overflow; live messages were dropped');
        const receipt=++this.channelTicket;
        this.channelDeliveries.set(receipt,{session,bytes});
        this.channelSessions.set(session,previous+bytes);this.channelBytes+=bytes;
        return {...frame,receipt};
    }
    channelConsumed(receipt:number):void {
        const held=this.channelDeliveries.get(receipt);if(!held)return;
        this.channelDeliveries.delete(receipt);this.channelBytes-=held.bytes;
        const remaining=this.channelSessions.get(held.session)!-held.bytes;
        if(remaining)this.channelSessions.set(held.session,remaining);else this.channelSessions.delete(held.session);
    }
    // Lazy background discovery: ordinary baseline operations add no round trip.
    describe(): Promise<ApDescription> {
        if (!this.description) {
            const transport = this.transport;
            const pending = (async () => {
                const handle = await this.begin({ op: "describe", operation_id: encodeId(crypto.getRandomValues(new Uint8Array(16))) });
                try {
                    const response = await this.streamStep(handle, async state => this.responseBody(state,
                        await this.serviceFrame(state, MAX_SERVICE_DESCRIPTION)));
                    const value = response.data as ApDescription;
                    const names = (values: unknown, feature: boolean): values is string[] => Array.isArray(values)
                        && values.length <= 64 && new Set(values).size === values.length
                        && values.every(name => typeof name === "string" && name.length <= 64
                            && (feature ? /^[a-z0-9._-]+$/ : /^[a-z0-9_]+$/).test(name));
                    if (!value || value.protocol !== SERVICE_PROTOCOL || !names(value.operations, false)
                        || !value.operations.includes("describe") || !names(value.features, true)
                        || value.algorithms?.kvdb_pull !== "kvdb.pull.v1" || value.algorithms.kvdb_delta !== "kvdb.delta.v1"
                        || value.algorithms.replica_events !== "ap.replica.v1") throw new Error("Invalid AP discovery response");
                    for (const [key, cap] of Object.entries({ frame_bytes: 1048576, request_bytes: MAX_SERVICE_REQUEST,
                        required_features: 16, feature_name_bytes: 64 })) {
                        const limit = value.limits?.[key as keyof ApDescription["limits"]];
                        if (!Number.isSafeInteger(limit) || limit < (key === "required_features" ? 0 : 1) || limit > cap)
                            throw new Error("Invalid AP discovery limits");
                    }
                    if (transport !== this.transport) throw failure("ap-transport", "AP connection changed during discovery");
                    this.sessionDisplayNames = value.features.includes('session.display-names.v1');
                    return value;
                } finally { this.streamClose(handle); }
            })();
            this.description = pending;
            void pending.catch(() => { if (this.description === pending) this.description = null; });
        }
        return this.description;
    }

    async begin(operation: ApOperation, generation?: number): Promise<number> {
        if (generation !== undefined && (!this.status().connected || generation !== this.connectionCount)) throw failure('bloblog-offline', 'Append connection is unavailable');
        if (operation.op === 'publish_bloblog' || operation.op === 'get_bloblog') operation = { ...operation, required_features: ['bloblog-append-v1'] };
        if (!this.ready) throw failure("ap-transport", "AP is disconnected; reconnect before retrying pending work");
        await this.ready;
        const transport = this.transport;
        if (!transport || generation !== undefined && generation !== this.connectionCount) throw failure(generation === undefined ? "ap-transport" : "bloblog-offline", "AP is disconnected");
        const required = operation.required_features ?? [];
        if (!Array.isArray(required) || required.length > 16 || new Set(required).size !== required.length
            || required.some(name => typeof name !== "string" || name.length > 64 || !/^[a-z0-9._-]+$/.test(name)))
            throw new Error("Invalid required AP features");
        if (required.length) {
            const description = await this.describe();
            if (required.some(name => !description.features.includes(name)))
                throw Object.assign(failure("ap-rejected", "Required AP feature is unavailable"), { serviceCode: "unsupported_feature" });
        }
        if (transport !== this.transport) throw failure(generation === undefined ? "ap-transport" : "bloblog-offline", "AP connection changed before admission");
        const body = utf8.encode(JSON.stringify(operation));
        if (body.length > MAX_SERVICE_REQUEST) throw new Error("AP request exceeds limit");
        const advisory = advisoryOperation('begin', operation);
        if (advisory && this.advisory.has(advisory)) throw failure('ap-capacity', 'An advisory AP operation is already active');
        if (!advisory && this.active - this.advisory.size >= 16) throw new Error("AP has too many concurrent operations");
        if (!validId(operation.operation_id, 16)) throw new Error("Invalid AP operation identity");
        const handle = this.nextStream++;
        const state: StreamState = { operation, busy: false, closed: false, finished: false, handle, advisory };
        if (advisory) this.advisory.add(advisory);
        this.streams.set(handle, state);
        this.active++;
        try {
            await this.streamStep(handle, async () => {
                const stream = await transport.createBidirectionalStream().catch(error => { throw generation === undefined ? transportFailure(error) : failure("bloblog-offline", "AP stream was not opened"); });
                state.writer = stream.writable.getWriter();
                state.reader = new FrameReader(stream.readable);
                if (state.closed || transport !== this.transport || generation !== undefined && generation !== this.connectionCount) { this.abandonStream(state); throw failure(generation === undefined ? "ap-transport" : "bloblog-offline", "AP operation retired before sending"); }
                // `ready` is only ever set alongside `identity`.
                await admittedWrite(state.writer, encodeFrame({ type: AP.DIRECT, flags: SERVICE_PROTOCOL, source: this.identity!.instanceId,
                    destination: utf8.encode("service"), body })).catch(error => { throw transportFailure(error); });
            });
            return handle;
        } catch (error) { this.streamClose(handle); throw error; }
    }

    abandonStream(state: StreamState): void {
        state.upload?.cancel(new Error("AP stream closed")).catch(() => {});
        state.writer?.abort().catch(() => {}).finally(() => { try { state.writer!.releaseLock(); } catch {} });
        state.reader?.close().catch(() => {});
    }

    streamClose(handle: number): void {
        const state = this.streams.get(handle);
        if (!state) return;
        this.streams.delete(handle);
        clearTimeout(state.timer);
        state.closed = true;
        if (state.advisory) this.advisory.delete(state.advisory);
        if (state.replica) this.replicaStreams--;
        else if (!state.channel) this.active--;
        this.abandonStream(state);
    }

    async streamStep<T>(handle: number, work: (state: StreamState) => Promise<T>): Promise<T> {
        const state = this.streams.get(handle);
        if (!state || state.closed) throw failure("ap-transport", "AP stream is closed");
        if (state.busy) throw new Error("AP stream already has an operation in progress");
        state.busy = true;
        clearTimeout(state.timer);
        try {
            return state.channel ? await work(state) : await deadline(Promise.resolve().then(() => work(state)), this.operationTimeout, () => this.streamClose(handle));
        } catch (error) { this.streamClose(handle); throw error; }
        finally {
            state.busy = false;
            if (!state.closed && !state.channel) state.timer = setTimeout(() => this.streamClose(handle), this.operationTimeout);
        }
    }

    async serviceFrame(state: StreamState, maxBody?: number): Promise<Frame> {
        const frame = await state.reader!.frame(maxBody);
        if (frame.type === AP.ERROR && frame.flags === 4 && text.decode(frame.source) === "service")
            throw failure("ap-rejected", "Playbit and its sync service need compatible updates before they can connect. Your saved data has been kept.");
        if (frame.type >= AP.ERROR) throw failure("ap-rejected", `AP rejected operation: ${text.decode(frame.body)}`);
        if (frame.type !== AP.DIRECT || text.decode(frame.source) !== "service") throw new Error("Invalid AP service data frame");
        return frame;
    }

    responseBody(state: StreamState, frame: Frame, reviver?: Parameters<typeof JSON.parse>[1]): ApResponse {
        if (frame.flags !== SERVICE_PROTOCOL) throw failure("ap-rejected",
            "Playbit and its sync service need compatible updates before they can connect. Your saved data has been kept.");
        // Service JSON: view it as a candidate record for the checks below.
        const response = JSON.parse(text.decode(frame.body), reviver) as { operation_id?: unknown; ok?: unknown; code?: unknown; error?: string; timing?: unknown; denial?: { cause?: unknown; account_id?: unknown; volume_id?: unknown; session_id?: unknown; build_id?: unknown } };
        if (response.operation_id !== state.operation.operation_id) throw new Error("AP response operation identity differs");
        validateApTiming(response.timing);
        if (response.ok !== true) {
            if (typeof response.code !== "string" || !/^[a-z0-9_]{1,64}$/.test(response.code)) throw new Error("Invalid AP failure code");
            let code = "ap-rejected";
            if (response.denial == null && response.code === 'operation_failed' && response.error === 'AP request capacity exhausted') {
                code = 'ap-capacity';
            } else if (state.operation.op === 'publish_bloblog' && response.denial) {
                const denied = response.denial;
                if (response.code !== 'operation_failed' || denied.cause !== 'participant_revoked'
                    || Object.keys(denied).sort().join() !== 'account_id,build_id,cause,session_id,volume_id'
                    || !validId(denied.account_id, 16) || !validId(denied.volume_id, 16)
                    || !(denied.session_id === null && denied.build_id === null
                        || validId(denied.session_id, 32) && validId(denied.build_id, 32)))
                    throw new Error('Invalid AP denial');
                code = 'bloblog-denied';
            }
            throw Object.assign(failure(code, response.error ?? "AP operation failed"), { serviceCode: response.code });
        }
        return response as ApResponse;
    }

    streamResponse(handle: number): Promise<ApResponse> {
        return this.streamStep(handle, async state => {
            return this.responseBody(state, await this.serviceFrame(state),
                state.operation.op === "sync_database" ? snapshotInteger
                    : state.operation.op === 'get_bloblog' ? bloblogInteger : undefined);
        });
    }

    async subscribeReplica(operation: ApOperation): Promise<ApReplicaSubscription> {
        if (!["subscribe_replica", "subscribe_account"].includes(operation.op)) throw new Error("Invalid replica subscription");
        if (this.replicaStreams >= 512) throw new Error("Replica subscription capacity exceeded");
        this.replicaStreams++;
        let handle: number | undefined, admitted = false;
        try {
            handle = await this.begin(operation);
            const ack = await this.streamStep(handle, async state => subscriptionAck(
                this.responseBody(state, await this.serviceFrame(state, 4096), replicaInteger).data));
            const state = this.streams.get(handle);
            clearTimeout(state!.timer); state!.replica = true; this.active--; admitted = true;
            return { handle, ack };
        } catch (error) {
            if (handle !== undefined) this.streamClose(handle);
            if (!admitted) this.replicaStreams--;
            throw error;
        }
    }

    async streamReplica(handle: number): Promise<Uint8Array> {
        const state = this.streams.get(handle);
        if (!state?.replica || state.closed) throw failure("ap-transport", "Replica stream is closed");
        if (state.busy) throw new Error("Replica read already in progress");
        state.busy = true;
        try {
            // Actual AP changes arrive here; no timer and no follow-up pull.
            const bytes = (await this.serviceFrame(state, MAX_REPLICA_EVENT)).body;
            replicaEvent(bytes);
            return bytes;
        } catch (error) { this.streamClose(handle); throw error; }
        finally { state.busy = false; }
    }

    async checkpointReplica(handle: number): Promise<void> {
        const state = this.streams.get(handle);
        if (!state?.replica || state.closed) throw failure("ap-transport", "Replica stream is closed");
        await this.writeControl(state, { type: AP.DIRECT, destination: utf8.encode("service"), body: new Uint8Array([1, 0]) });
    }
    private async writeControl(state: StreamState, frame: FrameInput): Promise<void> {
        if (state.controlWriting) throw new Error("Control write already in progress");
        state.controlWriting = true;
        try {
            // The single control writer is independent of an idle event read.
            await deadline(admittedWrite(state.writer!, encodeFrame(frame)).catch(error => { throw transportFailure(error); }),
                this.operationTimeout, () => this.streamClose(state.handle));
        } catch (error) { this.streamClose(state.handle); throw error; }
        finally { state.controlWriting = false; }
    }

    async publishReplica(operation: ApOperation, bytes: Uint8Array, generation?: number): Promise<ApPublicationAck> {
        if (!["publish_replica", "publish_account", "publish_bloblog"].includes(operation.op)
            || !(bytes instanceof Uint8Array) || !bytes.length || bytes.length > 65536) throw new Error("Invalid replica publication");
        const expires = Date.now() + this.operationTimeout;
        const handle = await this.begin(operation, generation);
        try {
            return await deadline((async () => {
                await this.streamWrite(handle, bytes);
                await this.streamFinish(handle);
                return await this.streamStep(handle, async state => {
                    // Service JSON: view it as a candidate record for the checks below.
                    const ack = this.responseBody(state, await this.serviceFrame(state, 4096), replicaInteger).data as Partial<ApPublicationAck> | null | undefined;
                    if (!ack || Object.keys(ack).sort().join() !== "database_clock,policy_revision"
                        || typeof ack.database_clock !== "bigint" || ack.database_clock <= 0n
                        || typeof ack.policy_revision !== "bigint" || ack.policy_revision <= 0n) throw new Error("Invalid durable publication acknowledgment");
                    return ack as ApPublicationAck;
                });
            })(), Math.max(1, expires - Date.now()), () => this.streamClose(handle));
        } finally { this.streamClose(handle); }
    }



    async watchInvitation(operation: ApOperation): Promise<number> {
        if (operation.op !== "watch_invitation") throw new Error("Invalid invitation subscription");
        const handle = await this.begin(operation);
        try {
            await this.streamStep(handle, async state =>
                this.responseBody(state, await this.serviceFrame(state, 4096)));
            clearTimeout(this.streams.get(handle)!.timer);
            return handle;
        } catch (error) { this.streamClose(handle); throw error; }
    }

    async streamInvitation(handle: number): Promise<ApInvitationEvent> {
        const state = this.streams.get(handle);
        if (!state || state.closed || state.operation.op !== "watch_invitation") throw new Error("Invitation stream is closed");
        if (state.busy) throw new Error("Invitation read already in progress");
        state.busy = true;
        try {
            // Idle invitation waiting has no ordinary request timeout. Transport
            // closure or explicit tab cancellation ends this bounded read.
            // Only parsed here; the consumer validates the fields.
            return JSON.parse(text.decode((await this.serviceFrame(state, 4096)).body)) as ApInvitationEvent;
        } catch (error) { this.streamClose(handle); throw error; }
        finally { state.busy = false; }
    }

    streamNamespaceEvidence(handle: number): Promise<Uint8Array> {
        return this.streamRead(handle, 8 + 4096 * 16);
    }

    streamRead(handle: number, maxBody?: number): Promise<Uint8Array> {
        return this.streamStep(handle, async state => {
            const frame = await this.serviceFrame(state, maxBody);
            // Bulk data uses flags zero. A service reply can terminate a rejected
            // transfer; validate its version before interpreting its JSON body.
            if (frame.flags !== 0) {
                this.responseBody(state, frame);
                throw new Error("Unexpected AP service reply during transfer");
            }
            return frame.body;
        });
    }

    streamWrite(handle: number, bytes: Uint8Array): Promise<void> {
        if (!(bytes instanceof Uint8Array) || bytes.length > 64 * 1024 * 1024) return Promise.reject(new Error("AP transfer exceeds buffer limit"));
        return this.streamStep(handle, async state => {
            if (state.finished) throw new Error("AP stream output is already finished");
            for (let offset = 0; offset < bytes.length; offset += 256 * 1024) {
                await admittedWrite(state.writer!, encodeFrame({ type: AP.DIRECT, destination: utf8.encode("service"),
                    body: bytes.subarray(offset, offset + 256 * 1024) })).catch(error => { throw transportFailure(error); });
            }
        });
    }

    streamFinish(handle: number): Promise<void> {
        return this.streamStep(handle, async state => {
            requireStorageFormat();
            if (state.finished) throw new Error("AP stream output is already finished");
            // AP can finish a request after its framed payload and stop reading
            // before FIN is acknowledged. The response (including the final
            // transfer acknowledgment) decides success, not this half-close.
            state.finished = true;
            void state.writer!.close().catch(() => {});
        });
    }

    async request(operation: ApOperation, upload?: ReadableStream<Uint8Array>, generation?: number): Promise<ApResponse> {
        const expires = Date.now() + this.operationTimeout;
        const handle = await this.begin(operation, generation);
        try {
            return await deadline((async () => {
                if (upload) {
                    await this.streamStep(handle, async state => {
                        // `isSafeInteger` already rejects non-numbers; the `typeof` only narrows for the comparisons.
                        if (typeof operation.content_length !== "number" || !Number.isSafeInteger(operation.content_length) || operation.content_length < 0
                            || operation.content_length > MAX_UPLOAD_BYTES) throw new Error("Invalid upload length");
                        state.upload = upload.getReader();
                        let sent = 0;
                        try {
                            for (;;) {
                                const { value, done } = await state.upload.read();
                                if (done) break;
                                if (!(value instanceof Uint8Array)) throw new Error("Invalid upload bytes");
                                sent += value.byteLength;
                                if (sent > operation.content_length) throw new Error("Upload exceeds declared length");
                                await admittedWrite(state.writer!, value).catch(error => { throw transportFailure(error); });
                            }
                            if (sent !== operation.content_length) throw new Error("Upload length differs from declaration");
                        } finally {
                            await state.upload.cancel().catch(() => {});
                            state.upload.releaseLock();
                            state.upload = null;
                        }
                    });
                }
                await this.streamFinish(handle);
                return await this.streamResponse(handle);
            })(), Math.max(1, expires - Date.now()), () => this.streamClose(handle));
        } finally { this.streamClose(handle); }
    }


}

function equalBytes(a:Uint8Array,b:Uint8Array):boolean {return a.length===b.length && a.every((byte,index)=>byte===b[index]);}
