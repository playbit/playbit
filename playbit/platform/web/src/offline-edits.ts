import type { OfflineEntry, RuntimeIdentity, RuntimeMethods } from './runtime-protocol.ts';

type Call = <M extends 'offline_entries' | 'offline_restore'>(method: M, args: RuntimeMethods[M]['args']) => Promise<RuntimeMethods[M]['result']>;

// Host-owned recovery stays outside guest app content and uses current authority
// on every request. Retained cuts remain available after restoring a fresh ID.
export function showOfflineEdits(container: HTMLElement, identity: RuntimeIdentity,
    personal: { id: string; name: string }[], call: Call): void {
    container.hidden = false;
    const form = document.createElement('form');
    const volume = document.createElement('select'), entries = document.createElement('select');
    const name = document.createElement('input'); name.type = 'text'; name.required = true; name.maxLength = 1024;
    const message = document.createElement('p'); message.setAttribute('role', 'status');
    const label = (text: string, control: HTMLElement) => {
        const node = document.createElement('label'); node.append(text, control); return node;
    };
    const button = (text: string) => { const node = document.createElement('button'); node.textContent = text; node.type = 'button'; return node; };
    const load = button('Load offline edits'), next = button('Next saved set'), restore = button('Restore as new entry');
    restore.type = 'submit'; next.disabled = true; restore.disabled = true; entries.size = 6;
    const description = document.createElement('p');
    description.textContent = 'Recover edits kept on this web browser after sharing permissions changed. Restore creates a separate entry; the saved edits remain available.';
    const choices = [{ id: identity.shared ?? identity.root, name: 'App data' }, ...personal];
    for (const item of choices) { const option = new Option(item.name || 'Untitled volume', item.id); volume.add(option); }
    const rows = document.createElement('div'); rows.className = 'row'; rows.append(load, next);
    const entryLabel = label('Saved edits', entries), nameLabel = label('New name', name);
    form.append(description, label('Volume', volume), rows, message, entryLabel, nameLabel, restore);
    container.replaceChildren(form);
    let items: OfflineEntry[] = [], cursor: string | null = null, busy = false;
    const selected = () => items[entries.selectedIndex];
    const ready = () => {
        volume.disabled = load.disabled = entries.disabled = name.disabled = busy;
        next.disabled = busy || cursor === null; restore.disabled = busy || !selected();
        entryLabel.hidden = nameLabel.hidden = restore.hidden = items.length === 0;
    };
    entries.onchange = () => { name.value = selected() ? `Recovered/${selected().entry.name}` : ''; ready(); };
    const read = async (after: string | null) => {
        if (busy) return;
        busy = true; ready(); message.textContent = 'Reading saved edits…';
        try {
            const page = await call('offline_entries', { volume: volume.value, after });
            items = page.entries; cursor = page.next; entries.replaceChildren();
            for (const item of items) entries.add(new Option(`${item.entry.name}${item.entry.deleted ? ' (deleted)' : ''}`, item.entry.id));
            if (items.length) entries.selectedIndex = 0;
            name.value = selected() ? `Recovered/${selected().entry.name}` : '';
            message.textContent = items.length ? 'Choose an entry and a new name.' : cursor ? 'No readable entries in this saved set.' : after ? 'No more saved edits on this web browser.' : 'No saved edits on this web browser.';
        } catch (error) { message.textContent = (error as Error).message; }
        finally { busy = false; ready(); }
    };
    load.onclick = () => { void read(null); }; next.onclick = () => { void read(cursor); };
    volume.onchange = () => { items = []; cursor = null; entries.replaceChildren(); name.value = ''; message.textContent = ''; ready(); };
    form.onsubmit = async event => {
        event.preventDefault(); const item = selected(); if (busy || !item) return;
        busy = true; ready(); message.textContent = 'Restoring saved edits…';
        try {
            await call('offline_restore', { volume: volume.value, cut: item.cursor.cut, entry: item.entry.id, name: name.value });
            message.textContent = `Restored as “${name.value}”. The original saved edits are retained.`;
        } catch (error) { message.textContent = (error as Error).message; }
        finally { busy = false; ready(); }
    };
    ready();
}
