open Playbit

let count = ref 0
let events = ref 0
let history = ref []

let render () =
    let view = View.create () in
    View.column_start view;
    View.text view TextStyle.title "OCaml on Playbit";
    View.text view TextStyle.body ("Count: " ^ string_of_int !count);
    View.text view TextStyle.muted (string_of_int !events ^ " events · Wasm GC");
    View.button view 1l ButtonStyle.primary "Increment";
    View.button view 2l ButtonStyle.default "Reset";
    View.column_end view;
    submit (View.finish view)

let update event =
    if Int32.compare event.kind (Int32.of_int EventKind.click) = 0 then begin
        if Int32.compare event.target 1l = 0 then incr count
        else if Int32.compare event.target 2l = 0 then count := 0;
        incr events;
        (* Transient closures/strings and a bounded list exercise collection. *)
        let describe n = "event " ^ string_of_int n ^ ": " ^ string_of_int !count in
        let values = List.init 256 describe in
        history := List.hd values :: (if List.length !history < 256 then !history else []);
        log ("Count: " ^ string_of_int !count ^ " (" ^ string_of_int !events ^ " events)")
    end

let turn () =
    let rec drain remaining =
        if remaining > 0 then match poll () with
        | None -> ()
        | Some event -> update event; drain (remaining - 1)
    in
    drain max_queued_events;
    render ()

let () =
    quit_on_last_window true;
    ignore (host_platform ());
    if host_limit (Int32.of_int HostLimit.view_bytes) < 1024l then
        failwith "Playbit view limit is too small";
    run turn
