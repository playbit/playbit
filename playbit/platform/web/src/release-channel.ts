// The same reviewed definition is compiled into native clients and web assets.
import endpoints from '../../../src/channels.json' with { type: 'json' };

export type ReleaseChannel = 'public' | 'testing';
export type Deployment = ReleaseChannel;
export const deployments: readonly Deployment[] = ['public', 'testing'];

export function channel(value: unknown): ReleaseChannel {
    if (value !== 'public' && value !== 'testing') throw new Error('Environment must be public or testing');
    return value;
}
export function configuration(value: Deployment) {
    const selected = channel(value);
    return { channel: selected, ...endpoints[selected] };
}
export function deploymentChannel(value: unknown, origin: string): Deployment {
    const selected = channel(value), expected = configuration(selected).website;
    // Plain local development retains its public-only origin. Testing uses its
    // separate HTTPS origin, before any credential or web profile opens.
    const local = selected === 'public' && /^http:\/\/(localhost|127\.0\.0\.1|\[::1\])(:[1-9][0-9]{0,4})?$/.test(origin);
    if (origin !== expected && !local) throw new Error(`Open ${expected}/s to use ${selected === 'testing' ? 'Testing' : 'Public'}. This origin belongs to a different environment`);
    return selected;
}
export function workerChannel(href: string): Deployment {
    const url = new URL(href);
    const [requested, ...duplicates] = url.searchParams.getAll('channel');
    if (requested === undefined || duplicates.length) throw new Error('Worker environment is missing or ambiguous');
    const deployment = deployments.find(candidate => candidate === requested);
    if (!deployment) throw new Error('Unknown worker environment');
    if (deploymentChannel(configuration(deployment).channel, url.origin) !== deployment) throw new Error('Worker belongs to a different environment');
    return deployment;
}
export function workerURL(path: string, base: string, selected: Deployment): URL {
    configuration(selected);
    const url = new URL(path, base);
    url.searchParams.set('channel', selected);
    return url;
}
