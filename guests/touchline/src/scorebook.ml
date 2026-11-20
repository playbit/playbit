open Playbit

(* The first access opens this session's Shared backing for live channels.
   The result also lives in the volume as ordinary, human-readable data. *)
type t = {
  mutable opening : Data.operation option;
  mutable entry : Data.entry option;
  mutable writable : bool;
  mutable writing : Data.operation option;
  mutable notice : string option;
}

let create () = { opening = None; entry = None; writable = false; writing = None; notice = None }

let open_session t ready =
  if t.opening = None then
    if t.entry <> None then ready ()
    else
      try
        let shared = Data.volume_info Data.Session in
        t.writable <- shared.can_write;
        t.opening <- Some (Data.resolve shared.volume "scorebook")
      with Host_error _ ->
        t.notice <- Some "Scorebook unavailable";
        ready ()

let complete t ready =
  (match t.opening with
   | None -> ()
   | Some op ->
       (try match Data.take_entry op with
        | Data.Pending -> ()
        | Data.Ready entry ->
            t.opening <- None;
            t.entry <- Some entry;
            t.notice <- None;
            ready ()
        with Host_error _ ->
          t.opening <- None;
          t.notice <- Some "Scorebook unavailable";
          ready ()));
  match t.writing with
  | None -> ()
  | Some op ->
      (try match Data.take_put op with
       | Data.Pending -> ()
       | Data.Ready () -> t.writing <- None
       with Host_error _ ->
         t.writing <- None;
         t.notice <- Some "Couldn’t save the final score")

let full_time t (game : Match.t) =
  match t.entry with
  | Some entry when t.writable && t.writing = None ->
      let score = "Portside FC " ^ string_of_int game.home_score ^ " – " ^ string_of_int game.away_score ^ " Rovers" in
      (try t.writing <- Some (Data.put entry "last-result" (Bytes.of_string score))
       with Host_error _ -> t.notice <- Some "Couldn’t save the final score")
  | _ -> ()
