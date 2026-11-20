// Static native-download metadata only. Runtime/account operations still use AP.
import { validId } from './identifier.ts';
import { configuration } from './release-channel.ts';
import type { ReleaseChannel } from './release-channel.ts';
// The element revealed once the manifest checks out (HTMLElement satisfies it).
export interface InstallContainer { hidden: HTMLElement["hidden"] }
export interface InstallLink { href: HTMLAnchorElement["href"] }
interface InstallManifest { channel?: string; artifact?: { platform?: string; filename?: string; sha256?: string; url?: string; sequence?: number; bytes?: number } }
export async function configureInstallation(container: InstallContainer, link: InstallLink, selected: ReleaseChannel, fetchAsset: typeof fetch = fetch): Promise<boolean> {
    container.hidden = true;
    link.href = "";
    try {
        const manifestURL = configuration(selected).macos_manifest;
        if (!manifestURL) return false;
        const response = await fetchAsset(manifestURL, { cache: "no-store", credentials: "omit", redirect: "error", signal: AbortSignal.timeout(10000) });
        if (!response.ok || !response.body) return false;
        const reader = response.body.getReader(), bytes = new Uint8Array(65536);
        let length = 0;
        try {
            for (;;) {
                const { value, done } = await reader.read();
                if (done) break;
                if (length + value.length > bytes.length) throw new Error("Download manifest exceeds 64 KiB");
                bytes.set(value, length); length += value.length;
            }
        } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
        const manifest: InstallManifest = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(0, length)));
        const artifact = manifest?.artifact, origin = "https://blob.playbit.app/";
        if (manifest?.channel !== selected || artifact?.platform !== "macos-arm64" || artifact.filename !== "Playbit-macos-arm64.zip"
            || typeof artifact.sha256 !== "string" || !/^[a-f0-9]{64}$/.test(artifact.sha256)
            || !Number.isSafeInteger(artifact.sequence) || artifact.sequence! < 1
            || !Number.isSafeInteger(artifact.bytes) || artifact.bytes! < 1 || artifact.bytes! > 256 * 1024 * 1024
            || typeof artifact.url !== "string" || !artifact.url.startsWith(origin) || !validId(artifact.url.slice(origin.length), 32)) return false;
        link.href = artifact.url;
        container.hidden = false;
        return true;
    } catch { return false; }
}
