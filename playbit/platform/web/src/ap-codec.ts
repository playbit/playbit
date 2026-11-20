import { failure, transportFailure } from "./errors.ts";
// Web encoding of the public archipelago-ap frame contract.
export const MAX_FRAME_LENGTH: number = 1024 * 1024;
export const AP = Object.freeze({ HELLO: 0, REGISTER: 1, DIRECT: 2, PUBLISH: 3, SUBSCRIBE: 4, KEY_CHALLENGE: 5, KEY_PROOF: 6, ONBOARDING: 7, AGREEMENT: 8, ERROR: 200 } as const);
// Pinned to the immutable Markdown offered by this version of the signup page.
export const AGREEMENT = Object.freeze({ revision: 1, sha256: "FkAwDnagbGhESYi4YqMD4RtihwgweQr9KXCAixygLy59" });
// Must match network::ap::service::PROTOCOL. Bulk data and event frames keep flags zero.
export const SERVICE_PROTOCOL = 1;
export const MAX_SERVICE_REQUEST = 393216;
export const MAX_SERVICE_DESCRIPTION = 16384;
export const utf8: TextEncoder = new TextEncoder();
export const text: TextDecoder = new TextDecoder("utf-8", { fatal: true });

// A frame to encode; absent fields default to zero flags, empty names and body.
export interface FrameInput { type: number; flags?: number; source?: Uint8Array; destination?: Uint8Array; body?: Uint8Array }
// A decoded frame with every field present.
export interface Frame { type: number; flags: number; source: Uint8Array; destination: Uint8Array; body: Uint8Array }
export interface RegistrationResponse { status: number; authToken: Uint8Array; instanceId: Uint8Array; caCertificate: Uint8Array; instanceCertificate: Uint8Array }

function validate(frame: FrameInput): void {
    const { type, flags = 0, source = new Uint8Array(), destination = new Uint8Array() } = frame;
    if (!Number.isInteger(type) || !((type >= 0 && type <= AP.AGREEMENT) || (type >= 200 && type <= 255))) {
        throw new Error("Invalid AP frame type");
    }
    if (!Number.isInteger(flags) || flags < 0 || flags > 255) throw new Error("Invalid AP flags");
    if (!(source instanceof Uint8Array) || !(destination instanceof Uint8Array)
        || source.length > 255 || destination.length > 255) throw new Error("Invalid AP name");
    if (type >= 2 && type <= 4 && !destination.length) throw new Error("Missing AP destination");
}

export function encodeFrame(frame: FrameInput): Uint8Array<ArrayBuffer> {
    validate(frame);
    const { type, flags = 0, source = new Uint8Array(), destination = new Uint8Array(), body = new Uint8Array() } = frame;
    if (!(body instanceof Uint8Array)) throw new Error("Invalid AP body");
    const offset = (8 + source.length + destination.length + 15) & ~15;
    if (offset + body.length > MAX_FRAME_LENGTH) throw new Error("AP frame exceeds limit");
    const bytes = new Uint8Array(offset + body.length);
    new DataView(bytes.buffer).setUint32(0, bytes.length, true);
    bytes.set([type, flags, source.length, destination.length], 4);
    bytes.set(source, 8);
    bytes.set(destination, 8 + source.length);
    bytes.set(body, offset);
    return bytes;
}

export function decodeFrame(bytes: Uint8Array): Frame {
    if (bytes.length < 8) throw new Error("Truncated AP header");
    const length = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0, true);
    if (length !== bytes.length || length > MAX_FRAME_LENGTH) throw new Error("Invalid AP frame length");
    const namesEnd = 8 + bytes[6] + bytes[7];
    const offset = (namesEnd + 15) & ~15;
    if (offset > length) throw new Error("Invalid AP header");
    if (bytes.subarray(namesEnd, offset).some(byte => byte !== 0)) throw new Error("Invalid AP padding");
    const frame = { type: bytes[4], flags: bytes[5], source: bytes.slice(8, 8 + bytes[6]),
        destination: bytes.slice(8 + bytes[6], namesEnd), body: bytes.slice(offset) };
    validate(frame);
    return frame;
}

// Owns the stream reader, retaining bytes delivered beyond the current frame.
export class FrameReader {
    reader: ReadableStreamDefaultReader<Uint8Array>;
    chunk: Uint8Array;
    offset: number;
    constructor(readable: ReadableStream<Uint8Array>) {
        this.reader = readable.getReader();
        this.chunk = new Uint8Array();
        this.offset = 0;
    }

    async exact(length: number): Promise<Uint8Array<ArrayBuffer>> {
        const result = new Uint8Array(length);
        let written = 0;
        while (written < length) {
            if (this.offset === this.chunk.length) {
                const { value, done } = await this.reader.read().catch(error => { throw transportFailure(error); });
                if (done) throw failure("ap-transport", "AP stream ended early");
                this.chunk = value;
                this.offset = 0;
            }
            const count = Math.min(length - written, this.chunk.length - this.offset);
            result.set(this.chunk.subarray(this.offset, this.offset + count), written);
            written += count;
            this.offset += count;
        }
        return result;
    }

    async frame(maxBody: number = MAX_FRAME_LENGTH): Promise<Frame> {
        const header = await this.exact(8);
        const length = new DataView(header.buffer).getUint32(0, true);
        if (length < 16 || length > MAX_FRAME_LENGTH) throw new Error("Invalid AP frame length");
        const bodyOffset = (8 + header[6] + header[7] + 15) & ~15;
        if (length - bodyOffset > maxBody) throw new Error("AP frame body exceeds limit");
        const bytes = new Uint8Array(length);
        bytes.set(header);
        bytes.set(await this.exact(length - 8), 8);
        return decodeFrame(bytes);
    }

    async close(): Promise<void> {
        try { await this.reader.cancel(); } finally { this.reader.releaseLock(); }
    }
}

export function registrationFrame(email: string, name: string, code: string, csr: Uint8Array = new Uint8Array()): FrameInput {
    const values = [email, name, code].map(value => utf8.encode(value));
    if (!values[0].length || !values[1].length || values[0].length > 65535
        || values[1].length > 255 || values[2].length > 255) throw new Error("Invalid registration fields");
    const body = new Uint8Array(4 + values.reduce((sum, value) => sum + value.length, 0) + csr.length);
    new DataView(body.buffer).setUint16(0, values[0].length, true);
    body[2] = values[1].length;
    body[3] = values[2].length;
    let offset = 4;
    for (const value of [...values, csr]) { body.set(value, offset); offset += value.length; }
    return { type: AP.REGISTER, flags: 0, body };
}

export function registrationResponse(frame: Frame): RegistrationResponse {
    if (frame.type !== AP.REGISTER || frame.body.length < 4) throw new Error("Invalid registration response");
    const body = frame.body;
    const tokenEnd = 4 + body[0];
    const idEnd = tokenEnd + body[1];
    const caEnd = idEnd + new DataView(body.buffer, body.byteOffset).getUint16(2, true);
    if (caEnd > body.length) throw new Error("Truncated registration response");
    return { status: frame.flags, authToken: body.slice(4, tokenEnd), instanceId: body.slice(tokenEnd, idEnd),
        caCertificate: body.slice(idEnd, caEnd), instanceCertificate: body.slice(caEnd) };
}

function der(tag: number, ...parts: Uint8Array[]): Uint8Array<ArrayBuffer> {
    const size = parts.reduce((sum, bytes) => sum + bytes.length, 0);
    const length = size < 128 ? [size] : size < 256 ? [0x81, size] : [0x82, size >> 8, size & 255];
    return Uint8Array.from([tag, ...length, ...parts.flatMap(bytes => Array.from(bytes))]);
}

// PKCS#10: version 0, empty subject, Ed25519 SPKI, empty attributes.
export async function certificateRequest(keyPair: CryptoKeyPair): Promise<Uint8Array<ArrayBuffer>> {
    const spki = new Uint8Array(await crypto.subtle.exportKey("spki", keyPair.publicKey));
    const info = der(0x30, Uint8Array.of(2, 1, 0), Uint8Array.of(0x30, 0), spki, Uint8Array.of(0xa0, 0));
    const signature = new Uint8Array(await crypto.subtle.sign("Ed25519", keyPair.privateKey, info));
    return der(0x30, info, Uint8Array.of(0x30, 5, 6, 3, 0x2b, 0x65, 0x70), der(3, Uint8Array.of(0), signature));
}
