open Playbit

type board = { round : int64; alice : int; bob : int }
let empty round = { round; alice = 0; bob = 0 }

(* One immutable key per award keeps simultaneous clicks independent. KV keys
   sort by bytes, so storing the complemented round first puts the current
   game first. A refresh can stop as soon as it reaches an older game. *)
let move_key round player =
  let nonce = if player = 'R' then Bytes.empty else Bytes.create 16 in
  if player <> 'R' then random_fill nonce;
  let key = Bytes.create (9 + Bytes.length nonce) in
  Bytes.set_int64_be key 0 (Int64.lognot round);
  Bytes.set key 8 player;
  Bytes.blit nonce 0 key 9 (Bytes.length nonce);
  Bytes.to_string key

let scores = ref None
let database = ref None
let writes = ref []
let pending_read = ref None
let refresh = ref true
let error = ref ""

(* Only one read is in flight. The host wakes the guest when it is ready. *)
let await take operation next =
  pending_read := Some (fun () ->
    match take operation with
    | Data.Pending -> ()
    | Data.Ready value -> pending_read := None; next value)

let rec read_scores entry cursor tally =
  await Data.take_keys (Data.keys ?cursor entry) (fun (keys, next) ->
    let rec count tally = function
      | [] ->
          (match next with
           | Some cursor -> read_scores entry (Some cursor) tally
           | None -> scores := Some (Option.value tally ~default:(empty 0L)))
      | key :: rest ->
          if String.length key < 9 then failwith "Invalid Scorekeeper move";
          let round = Int64.lognot (String.get_int64_be key 0) in
          let board = Option.value tally ~default:(empty round) in
          if round <> board.round then scores := Some board
          else
            let board = match key.[8], String.length key with
              | 'A', 25 -> { board with alice = board.alice + 5 }
              | 'B', 25 -> { board with bob = board.bob + 5 }
              | 'R', 9 -> board
              | _ -> failwith "Invalid Scorekeeper move"
            in
            count (Some board) rest
    in
    (* A click or notification during this page invalidates its snapshot. *)
    if not !refresh then count tally keys)

let start_refresh () =
  refresh := false;
  match !database with
  | Some entry -> read_scores entry None None
  | None ->
      let op = Data.resolve (Data.volume Data.Session) "scores" in
      await Data.take_entry op (fun entry ->
        database := Some entry;
        refresh := true)

let click target =
  match !database, !scores with
  | Some entry, Some board ->
      let save player updated =
        let op = Data.put entry (move_key updated.round player) Bytes.empty in
        writes := op :: !writes;
        scores := Some updated;
        refresh := true
      in
      (match target with
       | 1l -> save 'A' { board with alice = board.alice + 5 }
       | 2l -> save 'B' { board with bob = board.bob + 5 }
       | 3l -> save 'R' (empty (Int64.succ board.round))
       | _ -> ())
  | _ -> ()

let report code =
  error := "Couldn’t update the scoreboard (Playbit error " ^ Int32.to_string code ^ ")."

let complete () =
  writes := List.filter (fun op ->
    try Data.take_put op = Data.Pending
    with Host_error code -> report code; refresh := true; false) !writes;
  match !pending_read with
  | None -> ()
  | Some take ->
      try take () with
      | Host_error code ->
          pending_read := None;
          if code = Int32.of_int Error.conflict then refresh := true else report code
      | Failure message -> pending_read := None; error := message

let render () =
  let view = View.create () in
  View.column_start view;
  View.text view TextStyle.title "Friday table";
  View.text view TextStyle.muted "First to 50 points";
  (match !scores with
   | None -> if !error = "" then View.text view TextStyle.body "Loading scores…"
   | Some board ->
       View.text view TextStyle.body ("Alice   " ^ string_of_int board.alice ^ " points");
       View.text view TextStyle.body ("Bob     " ^ string_of_int board.bob ^ " points");
       View.row_start view;
       View.button view 1l ButtonStyle.primary "Alice +5";
       View.button view 2l ButtonStyle.default "Bob +5";
       View.row_end view;
       View.button view 3l ButtonStyle.default "Reset");
  if !error <> "" then (
    View.text view TextStyle.body !error;
    View.button view 4l ButtonStyle.default "Reload scores");
  View.column_end view;
  submit (View.finish view)

let turn () =
  let ready = ref false in
  let rec drain remaining =
    if remaining > 0 then match poll () with
    | None -> ()
    | Some event ->
        if event.kind = Int32.of_int EventKind.data_ready then ready := true
        else if event.kind = Int32.of_int EventKind.data_changed then refresh := true
        else if event.kind = Int32.of_int EventKind.click then (
          if event.target = 4l then (error := ""; database := None; refresh := true)
          else try click event.target with Host_error code -> report code);
        drain (remaining - 1)
  in
  drain max_queued_events;
  if !ready then complete ();
  if !refresh && !pending_read = None && !writes = [] then (
    try start_refresh () with Host_error code -> report code);
  render ()

let () =
  quit_on_last_window true;
  run turn
