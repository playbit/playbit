// Live session channels. AP owns membership; only participant hosts handle plaintext.
import { ChannelHost } from './channel-host.ts';
import { AP } from './ap-codec.ts';
import type { Frame, FrameInput } from './ap-codec.ts';
import { AbiError } from './guest-memory.ts';
import { failure } from './errors.ts';
const concat=(...parts:Uint8Array[]):Uint8Array<ArrayBuffer>=>{const result=new Uint8Array(parts.reduce((n,p)=>n+p.length,0));let at=0;for(const p of parts){result.set(p,at);at+=p.length;}return result;};
export interface ChannelTransport {
    connect():Promise<{device:Uint8Array;generation:number}>;
    open(frame:FrameInput,device:Uint8Array,generation:number):Promise<number>;
    read(handle:number):Promise<Frame & {receipt?:number}>;
    consumed(receipt:number):void;
    close(handle:number):Promise<unknown>;
}
interface Identity { session:Uint8Array;account:Uint8Array;device:Uint8Array;instance:Uint8Array;build:Uint8Array;proof:Uint8Array;app?:{publisher:string;app_id:string} }
export class SessionChannels {
    readonly identity:Identity;
    private readonly transport:ChannelTransport;
    private readonly protocol:ChannelHost;
    private readonly key:Promise<CryptoKey>;
    private own:Promise<number>|undefined;
    private readonly subscriptions=new Map<string,number>();
    private readonly desired=new Map<string,symbol>();
    private readonly opening=new Map<string,Promise<void>>();
    private epoch=0;
    private connected=false;
    private connectionGeneration:number|undefined;
    private retained=0;
    private readonly receipts=new Map<string,number[]>();
    private stopped=false;
    participants:Uint8Array[]= [];
    private readonly deliver:(name:string,source:Uint8Array,message:Uint8Array,data:Uint8Array)=>boolean|void;
    private readonly presence:()=>void;
    private readonly failed:(status:number)=>void;
    private readonly updated:()=>void;
    constructor(identity:Identity,key:Uint8Array<ArrayBuffer>,protocol:ChannelHost,transport:ChannelTransport,deliver:(name:string,source:Uint8Array,message:Uint8Array,data:Uint8Array)=>boolean|void,presence:()=>void,failed:(status:number)=>void,updated:()=>void){
        this.protocol=protocol;this.deliver=deliver;this.presence=presence;this.failed=failed;this.updated=updated;
        this.identity=identity;this.transport=transport;this.key=crypto.subtle.importKey('raw',key,'AES-GCM',false,['encrypt','decrypt']);
    }
    destination(name:string):Uint8Array<ArrayBuffer>{return this.protocol.destination(this.identity.session,name);}
    private frame(type:number,destination:Uint8Array,payload:Uint8Array):FrameInput{return {type,flags:1,destination,source:concat(this.identity.instance,this.identity.build),body:concat(this.identity.proof,payload)};}
    start():Promise<number>{
        if(this.stopped)return Promise.reject(new AbiError(-7));
        if(this.identity.app)return Promise.reject(failure("unsupported", "App-following sessions are not supported in the browser yet. Open this app in Playbit."));
        if(!this.own){
            const epoch=this.epoch;
            this.own=this.transport.connect().then(({device,generation})=>{
                if(this.stopped)throw new AbiError(-7);
                if(epoch!==this.epoch)throw failure('ap-transport','Session channel connection was superseded');
                this.identity.device=device;this.connectionGeneration=generation;
                return this.transport.open(this.frame(AP.SUBSCRIBE,this.identity.session,new Uint8Array()),this.identity.device,generation);
            }).then(handle=>{
                if(this.stopped||epoch!==this.epoch){void this.transport.close(handle);throw this.stopped?new AbiError(-7):failure('ap-transport','Session channel connection was superseded');}
                this.connected=true;
                void this.controls(handle,epoch).catch(()=>{if(epoch===this.epoch)this.disconnected(-5);});
                return handle;
            }).catch(error=>{if(epoch===this.epoch)this.disconnected(-5);throw error;});
        }
        return this.own;
    }
    private disconnected(status:number):void{
        if(this.stopped)return;
        this.epoch++;this.connected=false;
        const own=this.own;this.own=undefined;
        if(own)void own.then(handle=>this.transport.close(handle)).catch(()=>{});
        for(const handle of this.subscriptions.values())void this.transport.close(handle);
        this.subscriptions.clear();this.opening.clear();
        this.participants=[];this.presence();this.failed(status);
    }
    async reconnect(device:Uint8Array,generation:number):Promise<void>{
        if(this.stopped)return;
        if(this.connectionGeneration!==undefined&&this.connectionGeneration!==generation)this.disconnected(-5);
        this.identity.device=device;
        await this.start();
        for(const name of this.desired.keys())await this.subscribe(name);
    }
    private async controls(handle:number,epoch:number):Promise<void>{
        for(;;){
            const frame=await this.transport.read(handle);
            if(this.stopped||epoch!==this.epoch){if(frame.receipt!==undefined)this.transport.consumed(frame.receipt);return;}
            const control=this.protocol.receive(this.identity.session,frame);
            if(control.kind==='update'){this.updated();continue;}
            if(control.kind!=='presence')throw new Error('Expected Rust session control result');
            this.participants=control.participants;this.presence();
        }
    }
    async command(name:string,action:number,data:Uint8Array<ArrayBuffer>):Promise<void>{
        const destination=this.destination(name);this.protocol.command(name,action,data.length);
        if(action===1&&!this.connected)throw new AbiError(-5);
        if(action===1){const source=concat(this.identity.account,this.identity.device,this.identity.instance,this.identity.build),message=crypto.getRandomValues(new Uint8Array(16)),nonce=crypto.getRandomValues(new Uint8Array(12));
            const epoch=this.epoch;
            const cipher=new Uint8Array(await crypto.subtle.encrypt({name:'AES-GCM',iv:nonce,additionalData:this.protocol.aad(destination,source,message)},await this.key,data));
            if(!this.connected||epoch!==this.epoch)throw new AbiError(-5);
            let handle:number;
            try{handle=await this.transport.open(this.frame(AP.PUBLISH,destination,concat(message,nonce,cipher)),this.identity.device,this.connectionGeneration!);}catch{throw new AbiError(-15);}
            await this.transport.close(handle).catch(()=>{});return;
        }
        if(action===3||action===4){
            this.desired.delete(name);
            this.opening.delete(name);
            const current=this.subscriptions.get(name);
            this.subscriptions.delete(name);
            if(current!==undefined)await this.transport.close(current);
            return;
        }
        if(this.stopped)throw new AbiError(-7);
        if(!this.desired.has(name)&&this.desired.size>=16)throw new AbiError(-9);
        const intent=this.desired.get(name)??Symbol();
        this.desired.set(name,intent);
        await this.start();
        if(this.stopped||this.desired.get(name)!==intent)throw new AbiError(-7);
        await this.subscribe(name,intent);
    }
    private subscribe(name:string,intent=this.desired.get(name)):Promise<void>{
        if(intent===undefined)return Promise.resolve();
        if(this.subscriptions.has(name))return Promise.resolve();
        const pending=this.opening.get(name);if(pending)return pending;
        const epoch=this.epoch,destination=this.destination(name);
        const task=this.transport.open(this.frame(AP.SUBSCRIBE,destination,new Uint8Array()),this.identity.device,this.connectionGeneration!).then(handle=>{
            if(this.stopped||epoch!==this.epoch||this.desired.get(name)!==intent){void this.transport.close(handle);throw new AbiError(-7);}
            this.subscriptions.set(name,handle);
            void this.messages(name,handle,destination,epoch).catch(error=>{
                if(!this.stopped&&epoch===this.epoch&&this.subscriptions.get(name)===handle){
                    this.subscriptions.delete(name);void this.transport.close(handle);
                    this.failed(error instanceof AbiError?error.status:-5);
                }
            });
        }).finally(()=>{if(this.opening.get(name)===task)this.opening.delete(name);});
        this.opening.set(name,task);return task;
    }
    private deliveryKey(source:Uint8Array,message:Uint8Array):string{return Array.from(concat(source,message)).join(',');}
    private async messages(name:string,handle:number,destination:Uint8Array,epoch:number):Promise<void>{
        for(;;){
            let frame:Frame & {receipt?:number};
            try{frame=await this.transport.read(handle);}catch(error){if((error as {code?:string}).code==='channel-overflow')throw new AbiError(-9);throw error;}
            if(this.stopped||epoch!==this.epoch||this.subscriptions.get(name)!==handle){if(frame.receipt!==undefined)this.transport.consumed(frame.receipt);return;}
            try {
                const received=this.protocol.receive(destination,frame);
                if(received.kind!=='message')throw new Error('Expected Rust channel message result');
                if(this.retained+frame.body.length+frame.source.length+8>65536)throw new AbiError(-9);
                const message=frame.body.slice(0,16),nonce=frame.body.slice(16,28),aad=received.aad;
                const data=new Uint8Array(await crypto.subtle.decrypt({name:'AES-GCM',iv:nonce,additionalData:aad},await this.key,frame.body.slice(28)));
                if(this.stopped||epoch!==this.epoch||this.subscriptions.get(name)!==handle){if(frame.receipt!==undefined)this.transport.consumed(frame.receipt);return;}
                const key=this.deliveryKey(frame.source,message);
                if(frame.receipt!==undefined){const receipts=this.receipts.get(key)??[];receipts.push(frame.receipt);this.receipts.set(key,receipts);}
                this.retained+=data.length+104;
                try{if(this.deliver(name,frame.source,message,data)===false)this.consumed(data.length+104,frame.source,message);}
                catch{this.consumed(data.length+104,frame.source,message);throw new AbiError(-9);}
            } catch(error) {if(frame.receipt!==undefined)this.transport.consumed(frame.receipt);throw error;}
        }
    }
    consumed(bytes:number,source?:Uint8Array,message?:Uint8Array):void{
        this.retained=Math.max(0,this.retained-bytes);
        if(source&&message){const key=this.deliveryKey(source,message),receipts=this.receipts.get(key),receipt=receipts?.shift();if(!receipts?.length)this.receipts.delete(key);if(receipt!==undefined)this.transport.consumed(receipt);}
    }
    close():void{for(const receipts of this.receipts.values())for(const receipt of receipts)this.transport.consumed(receipt);this.receipts.clear();this.retained=0;this.stopped=true;this.epoch++;this.connected=false;this.desired.clear();for(const handle of this.subscriptions.values())void this.transport.close(handle);this.subscriptions.clear();if(this.own)void this.own.then(handle=>this.transport.close(handle)).catch(()=>{});}
}
