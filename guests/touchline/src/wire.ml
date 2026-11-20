(* Versioned live messages sent within one Playbit session. *)

type slots = string option array

type snapshot = {
    round : int;
    game : Match.t;
    slots : slots;
}

let vacant () = Array.make Match.player_count None
let peer_bytes = 48
let next_round_limit = 1_000_000_000

type packet =
    | Request of bool
    | Offer of string * snapshot
    | State of snapshot
    | Input of int * int * Match.input
    | Rematch of int
    | No_state of string

let round_valid round = round >= 1 && round <= next_round_limit

let encode packet =
    let snapshot prefix state =
        let mask = ref 0 in
        Array.iteri (fun slot peer ->
            if peer <> None then mask := !mask lor (1 lsl slot)
        ) state.slots;
        let header = Bytes.create 6 in
        Bytes.set_int32_le header 0 (Int32.of_int state.round);
        Bytes.set_uint16_le header 4 !mask;
        let out = Buffer.create 1200 in
        Buffer.add_string out prefix;
        Buffer.add_bytes out header;
        Array.iter (Option.iter (Buffer.add_string out)) state.slots;
        Buffer.add_string out (Match.encode state.game);
        Buffer.contents out
    in
    match packet with
    | Request takeover -> if takeover then "\001\002" else "\001\001"
    | Offer (peer, state) -> snapshot ("\001\003" ^ peer) state
    | State state -> snapshot "\001\004" state
    | No_state peer -> "\001\007" ^ peer
    | Rematch round ->
        let bytes = Bytes.create 6 in
        Bytes.blit_string "\001\006" 0 bytes 0 2;
        Bytes.set_int32_le bytes 2 (Int32.of_int round);
        Bytes.unsafe_to_string bytes
    | Input (round, shirt, input) ->
        let bytes = Bytes.create 12 in
        let axis value =
            int_of_float (Float.round (32767. *. max (-1.) (min 1. value)))
        in
        Bytes.set_uint8 bytes 0 1;
        Bytes.set_uint8 bytes 1 5;
        Bytes.set_int32_le bytes 2 (Int32.of_int round);
        Bytes.set_int16_le bytes 6 (axis input.dx);
        Bytes.set_int16_le bytes 8 (axis input.dy);
        Bytes.set_uint8 bytes 11 shirt;
        Bytes.set_uint8 bytes 10
            ((if input.pass then 1 else 0) lor (if input.shoot then 2 else 0));
        Bytes.unsafe_to_string bytes

let decode bytes =
    let length = String.length bytes in
    let read_round offset =
        let raw = String.get_int32_le bytes offset in
        let round = Int32.to_int raw in
        if Int32.of_int round = raw && round_valid round then Some round else None
    in
    let snapshot offset =
        if length < offset + 6 then None
        else Option.bind (read_round offset) (fun round ->
            let mask = String.get_uint16_le bytes (offset + 4) in
            if mask lsr Match.player_count <> 0 then None
            else (
                let position = ref (offset + 6) in
                let seen = ref [] in
                let seats = vacant () in
                let valid = ref true in
                for slot = 0 to Match.player_count - 1 do
                    if mask land (1 lsl slot) <> 0 then (
                        if !position > length - peer_bytes then valid := false
                        else (
                            let peer = String.sub bytes !position peer_bytes in
                            if List.mem peer !seen then valid := false;
                            seen := peer :: !seen;
                            seats.(slot) <- Some peer;
                            position := !position + peer_bytes
                        )
                    )
                done;
                if not !valid then None
                else Match.decode (String.sub bytes !position (length - !position))
                    |> Option.map (fun game -> { round; game; slots = seats })
            )
        )
    in
    if length < 2 || length > 1300 || bytes.[0] <> '\001' then None
    else match bytes.[1] with
    | '\001' when length = 2 -> Some (Request false)
    | '\002' when length = 2 -> Some (Request true)
    | '\003' when length >= 2 + peer_bytes ->
        snapshot (2 + peer_bytes)
        |> Option.map (fun state -> Offer (String.sub bytes 2 peer_bytes, state))
    | '\004' -> snapshot 2 |> Option.map (fun state -> State state)
    | '\005' when length = 12 ->
        let dx = String.get_int16_le bytes 6 in
        let dy = String.get_int16_le bytes 8 in
        let flags = String.get_uint8 bytes 10 in
        let shirt = String.get_uint8 bytes 11 in
        if dx < -32767 || dy < -32767 || flags > 3 || shirt >= Match.player_count
        then None
        else read_round 2 |> Option.map (fun round -> Input (round, shirt, {
            dx = float dx /. 32767.; dy = float dy /. 32767.;
            pass = flags land 1 <> 0; shoot = flags land 2 <> 0;
        }))
    | '\006' when length = 6 -> read_round 2 |> Option.map (fun round -> Rematch round)
    | '\007' when length = 2 + peer_bytes -> Some (No_state (String.sub bytes 2 peer_bytes))
    | _ -> None
