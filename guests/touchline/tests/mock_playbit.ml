exception Host_error of int32

module Error = struct
    let unavailable = -5
    let limit_exceeded = -9
end

module EventKind = struct
    let presence_changed = 5
    let wake = 10
end

type event_data =
    | Empty
    | Ready of int
    | Message of string * string * string
    | Status of int32

type event = { kind : int32; data : event_data }

let current = ref ""
let members : string list ref = ref []
let now = ref 0L
let outgoing : (string option * event) Queue.t = Queue.create ()
let blocked = ref false
let publish_error = ref None
let operation = ref 0

let monotonic_ns () = !now
let wake_after _ = ()

module Data = struct
    type volume = unit
    type volume_info = { volume : volume; can_write : bool }
    type volume_role = Session
    type entry = string
    type operation = Open of entry | Write
    type 'a completion = Pending | Ready of 'a

    let writable = ref true
    let writes : (entry * string * string) list ref = ref []
    let volume_info Session = { volume = (); can_write = !writable }
    let resolve _ name = Open name
    let take_entry = function Open name -> Ready name | Write -> assert false
    let put entry key value =
        writes := (entry, key, Bytes.to_string value) :: !writes;
        Write
    let take_put = function Write -> Ready () | Open _ -> assert false
end

module Session = struct
    type channel = string
    type operation = int
    type completion = Pending | Ready

    let identity () = String.sub !current 0 16 ^ String.make 16 '\000' ^ String.sub !current 32 16
    let same_instance a b =
        String.sub a 0 16 = String.sub b 0 16 && String.sub a 32 16 = String.sub b 32 16
    let presence () = !members
    let open_channel name = name

    let begin_operation () =
        incr operation;
        Queue.add (Some !current, { kind = 16l; data = Ready !operation }) outgoing;
        !operation

    let subscribe _ =
        if !blocked then raise (Host_error (Int32.of_int Error.unavailable));
        begin_operation ()

    let publish channel data =
        match !publish_error with
        | Some error -> raise (Host_error (Int32.of_int error))
        | None ->
            Queue.add (None, { kind = 18l; data = Message (channel, !current, data) })
                outgoing;
            begin_operation ()

    let take _ = Ready
    let ready event = match event.data with Ready op -> Some op | _ -> None
    let message event = match event.data with
        | Message (channel, sender, bytes) -> Some (channel, sender, bytes)
        | _ -> None
    let status event = match event.data with Status error -> Some error | _ -> None
end
