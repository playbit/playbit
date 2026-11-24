open Playbit
let assert_invalid f =
    match f () with
    | exception Invalid_argument _ -> ()
    | _ -> failwith "accepted malformed input"
let assert_invalid_message expected f =
    match f () with
    | exception Invalid_argument message when message = expected -> ()
    | _ -> failwith "unexpected canvas validation result"
let words values =
    let bytes = Bytes.create (List.length values * 4) in
    List.iteri (fun index value -> Bytes.set_int32_le bytes (index * 4) value) values;
    bytes
let assert_host_error expected f =
    match f () with
    | exception Host_error code when code = Int32.of_int expected -> ()
    | _ -> failwith "unexpected host status"
let next_event kind =
    match poll () with
    | Some e when e.kind = Int32.of_int kind -> e
    | _ -> failwith "unexpected event kind"
let identity n = String.init 48 (fun i -> Char.chr (n + i))
let message_bytes = String.init 130 (fun i -> Char.chr ((i * 17) land 255))
type canvas = Buffer.t -> int32 -> width:int -> height:int -> continuous:bool -> View.item list -> unit
let released_canvas : canvas = View.canvas
let released_viewport_canvas : canvas = View.viewport_canvas
let () =
    assert_invalid (fun () -> ignore (Input.frame (Bytes.make 4 '\000')));
    assert_invalid (fun () -> ignore (Input.pointer (Bytes.create 15)));
    assert_invalid (fun () -> ignore (Input.frame (words [0x80000001l])));
    List.iter (fun values -> assert_invalid (fun () -> ignore (Input.key (words values))))
        [[0x80000001l;1l];[5l;0x80000001l]];
    List.iter (fun values -> assert_invalid (fun () -> ignore (Input.pointer (words values))))
        [[0l;0l;0x80000000l;0l];[0x80000001l;0l;0l;0l];[0l;0x80000001l;0l;0l];[0l;0l;0l;0x80000001l]];
    List.iter (fun values -> assert_invalid (fun () -> ignore (Input.surface (words values))))
        [[0x80000001l;280l;0x3f800000l;3l];[360l;0x80000001l;0x3f800000l;3l];[360l;280l;0x3f800000l;0x80000003l];[0x7fffffffl;280l;0x3f800000l;3l]];
    let pixels = Bytes.make (512 * 280 * 4) '\255' in
    Bytes.set pixels 0 '\018'; Bytes.set pixels 1 '\052'; Bytes.set pixels 2 '\086';
    let view = View.create () in
    assert_invalid (fun () -> View.canvas view 1l ~width:512 ~height:280 ~continuous:false
        [View.Rect {x=511;y=0;width=2;height=1;color=0l}]);
    if View.finish view <> "PBV1" then failwith "invalid canvas changed scene";
    View.column_start view;
    View.row_start view; View.button view 5l ButtonStyle.default "Rules"; View.row_end view;
    released_canvas view 9l ~width:512 ~height:280 ~continuous:true
        [View.Rect {x=1;y=2;width=3;height=4;color=0xff123456l};
         View.Text ({x=5;y=6;width=20;height=12;color=0xffabcdefl},9,true,"Pixel");
         View.Pixels ({x=0;y=0;width=512;height=280;color=0l},512,280,pixels)];
    View.column_end view;
    submit (View.finish view);
    let menu = View.create () in
    let child () = View.canvas_with_options ~extent:View.Viewport menu 0l ~width:321 ~height:217 ~continuous:false [] in
    assert_invalid (fun () -> View.context_menu menu [] child);
    assert_invalid (fun () -> View.context_menu menu [0l, "Invalid"] child);
    if View.finish menu <> "PBV1" then failwith "invalid menu changed scene";
    View.context_menu menu [0x80000001l, "Réessayer"; 0x7fffffffl, "Quit"] child;
    submit (View.finish menu);
    let viewport = View.create () in
    released_viewport_canvas viewport 19l ~width:640 ~height:479 ~continuous:true [];
    submit (View.finish viewport);
    let window = View.create () in
    assert_invalid (fun () -> View.window_size_constraints ~min_width:64.25 ~max_width:64. window);
    assert_invalid (fun () -> View.window_size_constraints ~min_height:(float_of_int max_window_canvas_edge +. 2.) window);
    assert_invalid (fun () -> View.window_size_constraints ~min_height:0.5 ~max_width:0.75 ~aspect_ratio:2. window);
    assert_invalid (fun () -> View.window_size_constraints ~min_width:0.75 ~max_height:0.25 ~aspect_ratio:2. window);
    assert_invalid (fun () -> View.window_size_constraints ~max_height:1e-100 window);
    assert_invalid (fun () -> View.window_size_constraints ~min_width:(-0.5) window);
    assert_invalid (fun () -> View.window_size_constraints ~aspect_ratio:Float.nan window);
    assert_invalid (fun () -> View.window_style window WindowStyle.(theme_bright lor theme_dark));
    assert_invalid (fun () -> View.theme window [ThemeColor.text, 0l; ThemeColor.text, 1l]);
    assert_invalid (fun () -> View.canvas_with_options ~extent:View.Viewport window 19l
        ~width:15360 ~height:9000 ~continuous:false []);
    assert_invalid (fun () -> View.canvas_with_options ~extent:View.Window window 19l
        ~width:(max_window_canvas_edge + 1) ~height:1 ~continuous:false []);
    View.window_style window WindowStyle.(titlebar lor closable lor transparent_titlebar lor hide_session_button);
    View.window_size_constraints ~min_width:320.25 ~min_height:0.5 ~max_width:(-0.)
      ~max_height:180.140625 ~aspect_ratio:(16. /. 9.) window;
    View.theme window [ThemeColor.window_background, 0x132d41ffl];
    View.canvas_with_options ~extent:View.Window ~clear_color:0xa1b2c3d4l
      window 19l ~width:15360 ~height:9000 ~continuous:false
      [View.Rect {x=15359;y=8999;width=1;height=1;color=0xffffffffl}];
    submit (View.finish window);
    (* Reuse one buffer to exercise aggregate admission at 32 MiB without
       serializing or allocating a 32 MiB scene in the managed-Wasm heap. *)
    let shared = Bytes.make (512 * 512 * 4) '\000' in
    let image = View.Pixels ({x=0;y=0;width=512;height=512;color=0l},512,512,shared) in
    let at_limit = List.init 32 (fun _ -> image) in
    let invalid_rect = View.Rect {x=512;y=0;width=1;height=1;color=0l} in
    let extra = View.Pixels ({x=0;y=0;width=1;height=1;color=0l},1,1,Bytes.make 4 '\000') in
    let limited = View.create () in
    assert_invalid_message "Playbit canvas item outside bounds" (fun () ->
        View.canvas limited 10l ~width:512 ~height:512 ~continuous:false (at_limit @ [invalid_rect]));
    assert_invalid_message "Playbit pixel buffer limit" (fun () ->
        View.canvas limited 10l ~width:512 ~height:512 ~continuous:false (at_limit @ [extra; invalid_rect]));
    if View.finish limited <> "PBV1" then failwith "rejected pixel canvas changed scene";
    let initial = Data.volume_info Data.Root in
    let volume = initial.volume in
    if not initial.can_write then failwith "writable root";
    List.iter (fun (role, writable) ->
        let selected = Data.volume_info role in
        if selected.can_write <> writable then failwith "automatic role write permission";
        if selected.volume <> volume then failwith "automatic role selected the wrong handle"
    ) Data.[Session, false; User, false; User_session, true; Local, false; Local_session, true];
    if Data.root_volume () <> volume then failwith "released Root flag changed";
    assert_host_error Error.not_found Data.root_volume;
    let released = Data.volume Data.Root in
    let refreshed = Data.volume_info Data.Root in
    if refreshed.can_write || not initial.can_write then failwith "volume rights snapshot";
    if released <> volume || compare refreshed.volume volume <> 0
       || Obj.tag (Obj.repr refreshed.volume) <> Obj.custom_tag
    then failwith "volume identity changed with its rights";
    assert_host_error Error.limit_exceeded Data.root_volume;
    if Session.identity () <> identity 0 then failwith "local identity fields";
    if (Session.local_participant ()).display_name <> Some "Zoë"
       || EventKind.identity_changed <> 0x80000015l then failwith "local name";
    assert_host_error Error.internal Session.identity;
    let info = Option.get (Session.info ()) in
    if info.internal_id <> String.make 32 'c' || info.key_secret <> Some "session-seed-v1!"
       || Session.code info <> Some "FFUk4yL7S6oHXq7rrK6a48" then failwith "session invitation";
    let zero = Option.get (Session.info ()) in
    if Session.code zero <> Some (String.make 22 '1') then failwith "all-zero invitation";
    let seedless = Option.get (Session.info ()) in
    if seedless.internal_id <> info.internal_id || seedless.key_secret <> None
       || Session.code seedless <> None then failwith "session without an invitation";
    if Session.info () <> None then failwith "missing session";
    assert_host_error Error.unsupported Session.info;
    assert_host_error Error.internal Session.info;
    Session.join ();
    if Session.participants () <> [] then failwith "roster before first update";
    ignore (next_event EventKind.presence_changed);
    let members = Session.participants () in
    if List.map (fun (p : Session.participant) -> p.identity) members <> List.init 17 identity
       || (List.hd members).display_name <> None || (List.nth members 1).display_name <> Some "小"
       then failwith "presence names after capacity retry";
    ignore (next_event EventKind.presence_changed);
    let renamed = Session.participants () in
    if List.map (fun (p : Session.participant) -> p.identity) renamed
       <> List.map (fun (p : Session.participant) -> p.identity) members
       || (List.nth renamed 1).display_name <> Some "月" then failwith "peer renamed";
    if Session.presence () <> [] then failwith "empty presence";
    let me = identity 0 in
    let other_device = String.sub me 0 16 ^ String.make 16 '!' ^ String.sub me 32 16 in
    if not (Session.same_instance me other_device)
       || Session.same_instance me (identity 1)
       || Session.same_instance me ("x" ^ String.sub me 1 47)
       || Session.same_instance me (String.sub me 0 47 ^ "x")
       || Session.same_instance "short" "short"
    then failwith "instance tuple comparison";
    assert_invalid (fun () -> ignore (Session.open_channel ""));
    assert_invalid (fun () -> ignore (Session.open_channel (String.make 224 'x')));
    let operation = Data.resolve volume "pixel-plays" in
    let channel = Session.open_channel "live/é" in
    let subscription = Session.subscribe channel in
    (match Session.take subscription with Session.Pending -> () | _ -> failwith "missing pending subscription");
    let data_ready = next_event EventKind.data_ready in
    if Session.ready data_ready = None || Session.ready data_ready = Some subscription then
        failwith "DataReady misidentified an unrelated Data operation";
    (match Data.take_entry operation with Data.Pending -> () | _ -> failwith "missing pending entry");
    let entry = match Data.take_entry operation with Data.Ready e -> e | _ -> failwith "entry not ready" in
    let channel_ready = next_event EventKind.data_ready in
    if Session.ready channel_ready <> Some subscription then failwith "subscription DataReady handle";
    (match Session.take subscription with Session.Ready -> () | _ -> failwith "subscription not ready");
    assert_host_error Error.limit_exceeded (fun () -> Session.publish channel (String.make 32769 'a'));
    let publication = Session.publish channel message_bytes in
    let reported = next_event EventKind.channel_message in
    if Session.message reported <> Some (channel, identity 1, message_bytes) then
        failwith "channel message sender or binary payload";
    let ready = next_event EventKind.data_ready in
    if Session.ready ready <> Some publication then failwith "publication DataReady handle";
    (match Session.take publication with Session.Ready -> () | _ -> failwith "publication not ready");
    let failed = Session.publish channel "" in
    assert_host_error Error.uncertain (fun () -> Session.take failed);
    let stream = next_event EventKind.channel_status in
    if Session.status stream <> Some (Int32.of_int Error.unavailable) then failwith "channel stream status";
    List.iter (fun handle ->
        let broken e =
            let data = Bytes.copy e.data in
            Bytes.set_int64_le data 0 handle;
            { e with data } in
        if Session.ready (broken channel_ready) <> None || Session.message (broken reported) <> None then
            failwith "accepted malformed channel or operation handle"
    ) [0L; 1L; 0x100000000L; Int64.min_int; 0x8000000000000001L];
    if Session.ready { channel_ready with target = 1l } <> None
       || Session.ready { channel_ready with data = Bytes.create 9 } <> None
       || Session.message { reported with target = 1l } <> None
       || Session.message { reported with data = Bytes.create 103 } <> None
       || Session.status { stream with target = 1l } <> None
       || Session.status { stream with data = words [0l] } <> None
       || Session.status { stream with data = words [0x80000001l] } <> None
    then failwith "accepted malformed channel event";
    let click = next_event EventKind.click in
    if click.target <> 0x80000001l then failwith "context menu click target";
    let surface = Input.surface (next_event EventKind.surface_changed).data in
    if surface.width <> 641 || surface.height <> 480 || surface.pixel_scale <> 2.5 || surface.inputs <> 3 then
        failwith "viewport surface dimensions or scale";
    let await_channel operation =
        if Session.ready (next_event EventKind.data_ready) <> Some operation then
            failwith "channel command DataReady handle";
        match Session.take operation with
        | Session.Ready -> ()
        | _ -> failwith "channel command not ready" in
    let unsubscribe = Session.unsubscribe channel in
    (match Session.take unsubscribe with Session.Pending -> () | _ -> failwith "unsubscribe should be pending");
    await_channel unsubscribe;
    await_channel (Session.subscribe channel);
    let closing = Session.close channel in
    assert_host_error Error.stale_handle (fun () -> Session.subscribe channel);
    let reopened = Session.open_channel "live/é" in
    if reopened = channel then failwith "closed channel handle reused without a new generation";
    await_channel closing;
    await_channel (Session.close reopened);
    wake_after 1000;
    wake_after 86_400_000;
    assert_host_error Error.invalid_argument (fun () -> wake_after 999);
    ignore (next_event EventKind.wake);
    if monotonic_ns () <> 0xfedcba9876543210L then failwith "clock timestamp bits changed";
    if poll () <> None then failwith "unexpected pending event";
    let page = Data.keys entry in
    (match Data.take_keys page with Data.Pending -> () | _ -> failwith "missing pending keys");
    let cursor = match Data.take_keys page with
      | Data.Ready (["a"; "b\000"], Some cursor) -> cursor
      | _ -> failwith "unexpected first KV page" in
    (match Data.take_keys (Data.keys ~cursor entry) with
     | Data.Ready ([], None) -> ()
     | _ -> failwith "unexpected final KV page");
    (match Data.take_keys (Data.keys ~cursor entry) with
     | exception Host_error code when code = Int32.of_int Error.conflict -> ()
     | _ -> failwith "stale KV cursor did not report a conflict");
    let operation = Data.read entry "best/stari-most" ~limit:8 in
    let value = match Data.take_read operation ~limit:8 with Data.Ready b -> b | _ -> failwith "read not ready" in
    if Bytes.get_int64_le value 0 <> 123L then failwith "KV output changed";
    let operation = Data.put entry "best/stari-most" value in
    (match Data.take_put operation with Data.Ready () -> () | _ -> failwith "put not ready");
    let pending = Data.read entry "best/stari-most" ~limit:8 in
    Data.dispose pending;
    let operation = Audio.open_begin ~preferred_sample_rate:44100 in
    (match Audio.open_take operation with Audio.Pending -> () | _ -> failwith "missing pending audio");
    let audio = match Audio.open_take operation with Audio.Ready a -> a | _ -> failwith "audio not ready" in
    if Audio.sample_rate audio <> 48000 || Audio.volume audio <> 0.5 then failwith "audio output properties";
    Audio.set_volume audio 0.75;
    let sound = Sound.open_pcm audio ~channel_count:2 ~sample_rate:44100 (Bytes.make 128 '\001') in
    let first = Sound.instance sound Int32.min_int and second = Sound.instance sound 1234l in
    Sound.play first ~gain:0.5 ~start:0x123456789L ~length:0x23456789aL ~flags:Sound.loop;
    Sound.set_gain first 0.25; Sound.set_balance first (-0.5);
    if Sound.tell first <> 0xfedcba9876543210L then failwith "sound timestamp bits changed";
    Sound.seek first 0x100000001L;
    Sound.play second ~gain:1. ~start:0L ~length:1000000L ~flags:0l;
    let denied = Sound.instance sound 1235l in
    (match Sound.play denied ~gain:1. ~start:0L ~length:1000000L ~flags:0l with
     | exception Host_error code when code = Int32.of_int Error.limit_exceeded -> ()
     | _ -> failwith "playback admission failure lost");
    Sound.stop first; Sound.stop second;
    assert_invalid (fun () -> Sound.play first ~gain:1. ~start:0L ~length:0L ~flags:0l);
    assert_invalid (fun () -> Sound.play first ~gain:1. ~start:0L ~length:1L ~flags:2l);
    assert_invalid (fun () -> Sound.set_balance first Float.nan);
    assert_invalid (fun () -> Sound.set_balance first 1.1);
    assert_invalid (fun () -> Sound.set_gain first Float.infinity);
    assert_invalid (fun () -> Audio.set_volume audio (-0.1));
    assert_invalid (fun () -> ignore (Sound.open_pcm audio ~channel_count:2 ~sample_rate:44100 (Bytes.create 3)));
    Sound.close sound; Audio.close audio;
    log "expanded bindings passed";
    run (fun () -> ())
