// Synchronous adapters to the shared Rust channel contract. Web code owns I/O.
import { AbiError } from './guest-memory.ts';
import type { Frame } from './ap-codec.ts';
export interface ChannelExports {
    memory: WebAssembly.Memory;
    pb_input(length:number):number;
    pb_output():number;
    pb_web_guest_epoch():bigint;
    pb_web_channel_open(length:number):number;
    pb_web_channel_find(length:number):number;
    pb_web_channel_name(generation:bigint,slot:number,action:number):number;
    pb_web_channel_close(generation:bigint,slot:number):number;
    pb_web_channel_command(length:number,action:number,bytes:number):number;
    pb_web_channel_destination(length:number):number;
    pb_web_channel_aad(length:number,destination:number):number;
    pb_web_channel_receive(length:number,expected:number,destination:number,source:number,kind:number):number;
}
const utf8=new TextEncoder(),text=new TextDecoder();
const displayNameText = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
function checked(status:number):number{if(status<0)throw new AbiError(status);return status;}
type Received =
    | { kind: 'update' }
    | { kind: 'presence'; participants: { source: Uint8Array<ArrayBuffer>; displayName: string }[] }
    | { kind: 'message'; aad: Uint8Array<ArrayBuffer> };
export class ChannelHost {
    private readonly host:()=>ChannelExports;
    constructor(host:()=>ChannelExports){this.host=host;}
    private call(parts:Uint8Array[],operation:(host:ChannelExports,length:number)=>number):Uint8Array<ArrayBuffer>{
        const host=this.host(),length=parts.reduce((sum,part)=>sum+part.length,0),pointer=host.pb_input(length);
        if(length&&!pointer)throw new AbiError(-9);
        const input=new Uint8Array(host.memory.buffer,pointer,length);let at=0;
        for(const part of parts){input.set(part,at);at+=part.length;}
        const size=checked(operation(host,length));
        return new Uint8Array(host.memory.buffer,host.pb_output(),size).slice();
    }
    epoch():bigint{return this.host().pb_web_guest_epoch();}
    open(name:Uint8Array):Uint8Array<ArrayBuffer>{return this.call([name],(host,length)=>host.pb_web_channel_open(length));}
    find(name:string):Uint8Array<ArrayBuffer>{return this.call([utf8.encode(name)],(host,length)=>host.pb_web_channel_find(length));}
    name(generation:bigint,slot:number,action=4):string{return text.decode(this.call([],host=>host.pb_web_channel_name(generation,slot,action)));}
    close(generation:bigint,slot:number):void{checked(this.host().pb_web_channel_close(generation,slot));}
    command(name:string,action:number,bytes:number):void{this.call([utf8.encode(name)],(host,length)=>host.pb_web_channel_command(length,action,bytes));}
    destination(session:Uint8Array,name:string):Uint8Array<ArrayBuffer>{return this.call([session,utf8.encode(name)],(host,length)=>host.pb_web_channel_destination(length));}
    aad(destination:Uint8Array,source:Uint8Array,message:Uint8Array):Uint8Array<ArrayBuffer>{return this.call([destination,source,message],(host,length)=>host.pb_web_channel_aad(length,destination.length));}
    receive(expected:Uint8Array,frame:Frame):Received{
        const result=this.call([expected,frame.destination,frame.source,frame.body],(host,length)=>host.pb_web_channel_receive(length,expected.length,frame.destination.length,frame.source.length,frame.type));
        switch(result[0]){
            case 1:return {kind:'update'};
            case 2: {
                const participants = [], view = new DataView(result.buffer, result.byteOffset, result.byteLength);
                for (let at = 1; at < result.length;) {
                    const source = result.slice(at, at + 80), length = view.getUint16(at + 80, true);
                    at += 82;
                    participants.push({ source, displayName: displayNameText.decode(result.subarray(at, at + length)) });
                    at += length;
                }
                return { kind: 'presence', participants };
            }
            case 3:return {kind:'message',aad:result.slice(1)};
            default:throw new Error('Invalid Rust channel result');
        }
    }
}
