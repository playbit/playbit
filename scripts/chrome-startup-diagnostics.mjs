// Linux-only failure evidence. Never read process arguments or environments.
import { open, readlink } from 'node:fs/promises';
import { execFile } from 'node:child_process';

export async function linuxChromeStartupSnapshot(pid) {
    const processRoot = Number.isSafeInteger(pid) && pid > 0 ? `/proc/${pid}` : '/proc/no-child';
    const files = [
        ['status', `${processRoot}/status`, /^(Name|State|Threads|VmPeak|VmRSS|voluntary_ctxt_switches|nonvoluntary_ctxt_switches):/],
        ['stat', `${processRoot}/stat`],
        ['io', `${processRoot}/io`],
        ['load', '/proc/loadavg'],
        ['memory', '/proc/meminfo', /^(MemAvailable|MemFree|SwapFree|SwapTotal):/],
        ['cpu_pressure', '/proc/pressure/cpu'],
        ['io_pressure', '/proc/pressure/io'],
        ['memory_pressure', '/proc/pressure/memory'],
    ];
    const rows = await Promise.all(files.map(async ([name, path, filter]) => {
        let file;
        try {
            file = await open(path, 'r');
            const buffer = Buffer.alloc(1024);
            const { bytesRead } = await file.read(buffer, 0, buffer.length, 0);
            const text = buffer.subarray(0, bytesRead).toString();
            return [name, { text: filter ? text.split('\n').filter(line => filter.test(line)).join('\n') : text,
                limited: bytesRead === buffer.length }];
        } catch (error) { return [name, { unavailable: error.code ?? 'unknown' }]; }
        finally { await file?.close(); }
    }));
    const executable = await readlink(`${processRoot}/exe`).catch(error => `unavailable: ${error.code}`);
    const installedPackage = await new Promise(resolve => {
        execFile('/usr/bin/dpkg-query', ['--show', '--showformat=${Version}', 'google-chrome-stable'],
            { timeout: 500, killSignal: 'SIGKILL', maxBuffer: 1024 }, (error, stdout) => resolve(error ? { unavailable: error.code ?? 'unknown' } : stdout.trim()));
    });
    return { executable, installed_google_chrome_stable: installedPackage, ...Object.fromEntries(rows) };
}
