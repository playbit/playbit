import { ApConnection } from "./ap-connection.ts";
import { observeStorageFormat, requireStorageFormat, finishWebStorageReset } from './storage-format.ts';
import { workerChannel } from './release-channel.ts';
import type { ApChannelMessage } from "./ap-connection.ts";
import { AccountNotices } from './account-notices.ts';
import { resetWebAccount } from './web-account.ts';
import type { ResetRequest } from './web-account.ts';
import type { ApRequest } from "./ap-protocol.ts";
import { advisoryOperation, type AdvisoryOperation } from './ap-protocol.ts';

// A tab's request as the worker reads it before `method` narrows it: the
// account-notice acknowledgment and `detach` carry no `id` or `account`, and
// the account/handle guards below read `args.account`/`args.handle` off any
// method's object args (`status` sends `args: undefined`, which no guard reaches).
type ApGuardArgs = { account?: string | null; handle?: number };
type ApWithGuardArgs<R> = R extends { args: object } ? R & { args: ApGuardArgs } : R;
type ApPortRequest = ApWithGuardArgs<ApRequest> & { id?: number; account?: string | null };
// `resetState` resumes an interrupted reset by token alone; only a fresh
// `resetWebAccount` names the initiating tab.
interface ResetAccountArgs { tab?: string; token: string | null; resume: boolean }
// Errors reach the catch clauses as `unknown`; the fields are read off any value.
type Errorish = { message?: unknown; code?: string; serviceCode?: string };
const MAX_ERROR_STACK = 4096;

// The SharedWorker global; the webworker library types `self` for any worker.
declare const self: SharedWorkerGlobalScope;

const selectedChannel = workerChannel(self.location.href);
let connection = new ApConnection(selectedChannel);
observeStorageFormat(() => { connection.transport?.close(); connection.disconnected(connection.transport); });
let accountNotices = new AccountNotices();
let resetCode: string | undefined;
let resetting: Promise<void> | null | undefined, resetError: string | null | undefined, completedReset: string | null | undefined, generation = 0;
async function resetAccount(args: ResetAccountArgs): Promise<void> {
    if (resetting) { await resetting.catch(() => {}); return; }
    if (args.token === completedReset) return;
    const previous = connection;
    previous.transport?.close(); previous.disconnected(previous.transport);
    // A resumed reset has no tab; resetWebAccount reads it only when !resume.
    resetting = resetWebAccount(args as ResetRequest, () => Promise.allSettled([...inFlight]), globalThis, async () => {
        await finishWebStorageReset();
        connection = new ApConnection(selectedChannel); accountNotices = new AccountNotices();
        completedReset = args.token; resetError = null; resetCode = undefined; generation++;
    });
    try {
        await resetting;
    } catch (error) { resetError = String((error as Errorish).message ?? error); resetCode = (error as Errorish).code; }
    finally { resetting = null; }
}
const inFlight = new Set<Promise<void>>();
const advisoryInFlight = new Set<AdvisoryOperation>();

self.onconnect = ({ ports: [port] }) => {
    const handles = new Set<number>();
    const channelReceipts = new Set<number>();
    let channelOwner=connection;
    const releaseChannels = () => {for(const receipt of channelReceipts)channelOwner.channelConsumed(receipt);channelReceipts.clear();};
    let portGeneration = generation;
    let detached = false;
    let invitationHandle: number | undefined, invitationOpening = false;
    const receive = (message: ApChannelMessage) => port.postMessage({ event: "channel", ...message });
    port.onmessage = async ({ data }: MessageEvent<ApPortRequest>) => {
        const { id, method, args } = data;
        if (detached) return;
        if (method === 'resetWebAccount' || method === 'resetState') {
            if (method === 'resetWebAccount' || args.token) {
                await resetAccount({ ...args, resume: method === 'resetState' });
            } else if (resetting) await resetting.catch(() => {});
            // A reloaded page can attach while cleanup is pending. Old stream
            // handles must never address a new account's reused handle numbers.
            channelReceipts.clear();channelOwner=connection;
            handles.clear(); accountNotices.detach(port); portGeneration = generation;
            port.postMessage({ id, result: { error: resetError, code: resetCode } }); return;
        }
        if (portGeneration !== generation) {
            port.postMessage({ id, error: 'This web browser has signed out. Reload to continue' }); return;
        }
        if (resetting || resetError) {
            port.postMessage({ id, error: resetError || 'Clearing web browser data…' }); return;
        }
        const advisory = advisoryOperation(method, method === 'begin' || method === 'request' ? args?.operation : undefined);
        if (advisory && advisoryInFlight.has(advisory)) {
            port.postMessage({ id, code: 'ap-capacity', error: 'An advisory AP request is already pending' }); return;
        }
        if (!advisory && inFlight.size - advisoryInFlight.size >= 128 && method !== "channelConsumed") {
            port.postMessage({ id, error: 'Playbit is busy. Try again after the current operation finishes' }); return;
        }
        let finished!: () => void;
        if (advisory) advisoryInFlight.add(advisory);
        const task = new Promise<void>(resolve => { finished = resolve; }); inFlight.add(task);
        try {
            if (!['streamClose', 'channelConsumed', 'detach', 'status'].includes(method)) requireStorageFormat();
            let result;
            if (data.account && connection.account && data.account !== connection.account) throw new Error("AP worker belongs to another account");
            if (["watchAccount", "accountChanged", "ackAccountNotice"].includes(method)
                && (!connection.account || args!.account !== connection.account)) throw new Error("Account notice belongs to another account");
            // The account-notice methods above and every stream*/checkpointReplica
            // method send object args with the field each guard reads.
            if ((method.startsWith("stream") || method === "checkpointReplica" || method === "channelRead"
                || method === "channelDisplayName") && !handles.has(args!.handle!)) throw new Error("AP stream belongs to another tab");
            switch (method) {
                case "describe": result = await connection.describe(); break;
                case "onboarding": await connection.onboarding(args.stage); break;
                case "agreement": result = await connection.agreement(args); break;
                case "watchAccount": accountNotices.watch(port, args.account); break;
                case "accountChanged": accountNotices.changed(args.account); break;
                case "ackAccountNotice": accountNotices.acknowledge(port, args.ticket); return;
                case "requestCode": result = await connection.requestCode(args); break;
                case "signInWithKey": result = await connection.signInWithKey(args); break;
                case "finishCode": result = await connection.finishCode(args); break;
                case "beginChannel": {
                    const owner=connection;result=await owner.beginChannel(args.frame,args.device,args.generation);
                    if(detached||connection!==owner){owner.streamClose(result);return;}
                    handles.add(result);break;
                }
                case "channelDisplayName": result = await connection.channelDisplayName(args.handle, args.name); break;
                case "channelRead": {
                    const owner=connection;result=await owner.channelRead(args.handle);
                    if(result.receipt!==undefined){
                        if(detached||connection!==owner){owner.channelConsumed(result.receipt);return;}
                        channelReceipts.add(result.receipt);
                    }
                    break;
                }
                case "channelConsumed":
                    if(!Array.isArray(args.receipts)||args.receipts.length>1024)throw new Error("Invalid channel receipt batch");
                    for(const receipt of args.receipts)if(channelReceipts.delete(receipt))channelOwner.channelConsumed(receipt);
                    break;
                case "begin": result = await connection.begin(args.operation); handles.add(result); break;
                case "subscribeReplica":
                    result = await connection.subscribeReplica(args.operation);
                    if (detached) { connection.streamClose(result.handle); return; }
                    handles.add(result.handle); break;
                case "streamReplica": result = await connection.streamReplica(args.handle); break;
                case "checkpointReplica": result = await connection.checkpointReplica(args.handle); break;
                case "publishReplica": result = await connection.publishReplica(args.operation, new Uint8Array(args.bytes), args.generation); break;
                case "watchInvitation":
                    if (invitationOpening || invitationHandle !== undefined && handles.has(invitationHandle)) throw new Error("An invitation observer is already open in this tab");
                    invitationOpening = true;
                    try {
                        result = await connection.watchInvitation(args.operation);
                        if (detached) { connection.streamClose(result); return; }
                        invitationHandle = result; handles.add(result);
                    } finally { invitationOpening = false; }
                    break;
                case "streamInvitation": result = await connection.streamInvitation(args.handle); break;
                case "streamRead": result = await connection.streamRead(args.handle, args.maxBody); break;
                case "streamNamespaceEvidence": result = await connection.streamNamespaceEvidence(args.handle); break;
                case "streamWrite": result = await connection.streamWrite(args.handle, new Uint8Array(args.bytes)); break;
                case "streamFinish": result = await connection.streamFinish(args.handle); break;
                case "streamResponse": result = await connection.streamResponse(args.handle); break;
                case "streamClose": connection.streamClose(args.handle); handles.delete(args.handle); break;
                case "connect": result = await connection.connect(args); break;
                case "request": result = await connection.request(args.operation, args.upload, args.generation); break;
                case "status": result = connection.status(); break;
                case "subscribe": result = await connection.subscribe(args.channel, receive); break;
                case "publish": result = await connection.publish(args.channel, new Uint8Array(args.bytes)); break;
                case "detach": detached = true; releaseChannels(); accountNotices.detach(port); for (const handle of handles) connection.streamClose(handle); connection.detach(receive); port.close(); return;
                default: throw new Error("Unknown AP worker operation");
            }
            if (!detached) {
                if (method === 'streamRead' && result instanceof Uint8Array && result.buffer instanceof ArrayBuffer) {
                    port.postMessage({ id, result }, [result.buffer]);
                } else port.postMessage({ id, result });
            }
        } catch (error) {
            if (args?.handle !== undefined && !connection.streams.has(args.handle)) handles.delete(args.handle);
            if (!detached) port.postMessage({ ...(method === "ackAccountNotice" ? { event: "account-error" } : { id }),
                error: String((error as Errorish)?.message ?? error), code: (error as Errorish)?.code, serviceCode: (error as Errorish)?.serviceCode,
                stack: error instanceof Error ? error.stack?.slice(0, MAX_ERROR_STACK) : undefined });
        } finally {
            if (advisory) advisoryInFlight.delete(advisory);
            inFlight.delete(task); finished();
        }
    };
    port.addEventListener?.('close', () => {releaseChannels();accountNotices.detach(port);});
    port.start();
};
