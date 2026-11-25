// Native handoff must work even when experimental web execution cannot start.
import { isSessionPath, parseSessionCode, sessionUri } from "./invitation.ts";
import { configureInstallation } from "./install.ts";
import { channel, configuration, deploymentChannel } from './release-channel.ts';
import type { Deployment } from './release-channel.ts';

export function nativeInvitation(pathname: string, hash: string, selected: Deployment): string | null {
    if (!isSessionPath(pathname) || !hash.startsWith("#")) return null;
    try { return sessionUri(parseSessionCode(hash.slice(1)), selected); }
    catch { return null; }
}

if (typeof document !== "undefined") {
    let selected: Deployment;
    try { selected = deploymentChannel(document.querySelector<HTMLMetaElement>('meta[name="playbit-channel"]')?.content, location.origin); }
    catch (failure) {
        const message = document.querySelector<HTMLElement>('#error'), status = document.querySelector<HTMLElement>('#status');
        if (message) message.textContent = (failure as Error).message;
        if (status) status.textContent = 'This environment is unavailable';
        throw failure;
    }
    const deployment = configuration(selected);
    const personal = selected !== deployment.channel;
    const environment = document.querySelector<HTMLSelectElement>('#environment');
    if (environment) {
        environment.value = deployment.channel;
        if (personal) {
            const label = environment.closest('label');
            if (label) label.hidden = true;
        } else {
            environment.onchange = () => location.assign(configuration(channel(environment.value)).website + '/s');
        }
    }
    const badge = document.querySelector<HTMLElement>('#testing');
    if (badge) {
        badge.hidden = deployment.channel !== 'testing';
        if (personal) badge.textContent = `${selected.slice('testing-'.length)} · Testing`;
    }
    const preference = document.querySelector<HTMLElement>('#native-preference');
    if (preference) preference.hidden = personal;
    const instructions = document.querySelector<HTMLElement>('#install-instructions');
    if (instructions && selected === 'testing') instructions.textContent += ' In Playbit, choose Testing from Environment before reopening this invitation.';
    if (personal) {
        const intro = document.querySelector<HTMLElement>('.invitation-intro');
        if (intro) intro.textContent = 'Join this session with the experimental web runtime';
    }
    const diagnostics = document.querySelector<HTMLButtonElement>('#copy-environment');
    if (diagnostics) diagnostics.onclick = async () => {
        const failure = document.documentElement.dataset.replicationFailure;
        const text = `ReleaseChannel: ${deployment.channel}\nAP: ${deployment.ap_hostname}\nWeb assets: ${new URL('.', import.meta.url).pathname}`
            + (failure ? `\nLast sync failure: ${failure}` : '');
        try { await navigator.clipboard.writeText(text); diagnostics.textContent = 'Copied'; }
        catch { const detail = document.querySelector<HTMLElement>('#environment-details'); if (detail) { detail.textContent = text; detail.hidden = false; } }
    };
    const uri = nativeInvitation(location.pathname, location.hash, selected);
    if (uri) {
        const link = document.querySelector<HTMLAnchorElement>("#open-installed");
        if (link) { link.href = uri; link.hidden = false; }
        const title = document.querySelector<HTMLElement>("#invitation-title");
        if (title) title.textContent = "You're invited";
        if (!new URLSearchParams(location.search).has("app")) document.body.classList.add("invited");
    }
    const install = document.querySelector<HTMLElement>("#install-playbit");
    const download = document.querySelector<HTMLAnchorElement>("#download-playbit");
    if (!personal && install && download) {
        void configureInstallation(install, download, deployment.channel).then(available => {
            const footerDownload = document.querySelector<HTMLAnchorElement>('.footer-download');
            if (available && footerDownload) {
                footerDownload.href = download.href;
                footerDownload.hidden = false;
            }
        });
    }
    // A browser without runtime support can still use the native invitation.
    void import(new URL("./playbit.js", import.meta.url).href).catch(failure => {
        const error = document.querySelector<HTMLElement>("#error");
        if (error) error.textContent = `The web runtime could not start. ${failure.message}`;
        const status = document.querySelector<HTMLElement>("#status");
        if (status) status.textContent = "Browser participation is unavailable";
    });
}
