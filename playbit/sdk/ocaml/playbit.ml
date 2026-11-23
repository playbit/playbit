include Playbit_generated

exception Host_error of int32

external log_raw : string -> int64 = "caml_pb_log"
external submit_raw : string -> int64 = "caml_pb_view_submit"
external poll_raw : bytes -> bytes -> int64 = "caml_pb_event_poll"
external poll_deadline_raw : int64 -> bytes -> bytes -> int64 =
  "caml_pb_event_poll_deadline"
external host_limit_raw : int32 -> int64 = "caml_pb_host_limit"
external host_platform_raw : unit -> int64 = "caml_pb_host_platform"
external quit_raw : bool -> int64 = "caml_pb_quit_on_last_window"

let check result =
  if Int64.compare result 0L < 0 then
    let error =
      if Int64.compare result (-2147483648L) < 0 then Int32.of_int Error.internal
      else Int64.to_int32 result
    in
    raise (Host_error error)

let scalar32 result =
  check result;
  if Int64.compare result 2147483647L > 0 then
    raise (Host_error (Int32.of_int Error.internal));
  Int64.to_int32 result
let log text = check (log_raw text)
let submit scene = check (submit_raw scene)

let host_platform () =
  let result = host_platform_raw () in
  scalar32 result

let host_limit kind =
  let result = host_limit_raw kind in
  scalar32 result

let quit_on_last_window intent = check (quit_raw intent)
let optional_event_kind_bit = Int32.min_int

type event = { generation : int64; kind : int32; target : int32; data : bytes }

let header = Bytes.create event_bytes
let payload = ref (Bytes.create 128)
let unsupported_event = ref false

let rec poll_with poll_one =
  if !unsupported_event then raise (Host_error (Int32.of_int Error.unsupported));
  let result = poll_one header !payload in
  if Int64.compare result (Int64.of_int Error.would_block) = 0 then None
  else (
    check result;
    let length = Int32.to_int (scalar32 result) in
    if length > max_event_bytes then failwith "Playbit event exceeds ABI limit";
    let result =
      if length > Bytes.length !payload then (
        payload := Bytes.create length;
        let retry = poll_one header !payload in
        check retry;
        if Int64.compare retry result <> 0 then
          failwith "Playbit event changed during capacity retry";
        retry)
      else result
    in
    if
      Bytes.get_int32_le header event_size_offset <> Int32.of_int event_bytes
      || Bytes.get_int32_le header event_written_offset
         <> Int32.of_int event_bytes
    then failwith "Playbit event record is incomplete";
    let kind = Bytes.get_int32_le header event_kind_offset in
    let baseline = Int32.compare kind 1l >= 0 && Int32.compare kind 20l <= 0 in
    let optional =
      Int32.logand kind optional_event_kind_bit <> 0l
      && Int32.compare (Int32.logand kind Int32.max_int) 20l > 0
    in
    if (not baseline) && not optional then (
      unsupported_event := true;
      raise (Host_error (Int32.of_int Error.unsupported)));
    let target = Bytes.get_int32_le header event_target_offset in
    if kind = Int32.of_int EventKind.trace_config then (
      if target <> 0l || length <> 8 then
        failwith "Playbit trace configuration is malformed";
      (* This SDK has no tracing producer. Consume the host's callback snapshot
         without making applications handle an internal configuration event. *)
      poll_with poll_one)
    else
      Some
        {
          generation = Bytes.get_int64_le header event_generation_offset;
          kind;
          target;
          data = Bytes.sub !payload 0 (Int64.to_int result);
        })

let poll () = poll_with poll_raw
let poll_deadline deadline_ns = poll_with (poll_deadline_raw deadline_ns)

module Input = struct
  type pointer = { x : int; y : int; phase : int; buttons : int }
  type key = { key : int; pressed : bool }

  type surface = {
    width : int;
    height : int;
    pixel_scale : float;
    inputs : int;
  }

  let word bytes offset =
    let raw = Bytes.get_int32_le bytes offset in
    if
      Int32.compare raw 0l < 0
      || Int64.compare (Int64.of_int32 raw) (Int64.of_int max_int) > 0
    then invalid_arg "Playbit input word";
    Int32.to_int raw

  let pointer bytes =
    if Bytes.length bytes <> 16 then invalid_arg "Playbit pointer length";
    let value =
      {
        x = word bytes 0;
        y = word bytes 4;
        phase = word bytes 8;
        buttons = word bytes 12;
      }
    in
    if
      value.x < 0 || value.y < 0 || value.phase < 0 || value.phase > 3
      || value.buttons < 0 || value.buttons > 1
    then invalid_arg "Playbit pointer value";
    value

  let key bytes =
    if Bytes.length bytes <> 8 then invalid_arg "Playbit key length";
    let key = word bytes 0 and pressed = word bytes 4 in
    if key < 1 || key > 5 || pressed < 0 || pressed > 1 then
      invalid_arg "Playbit key value";
    { key; pressed = pressed = 1 }

  let frame bytes =
    if Bytes.length bytes <> 4 then invalid_arg "Playbit frame length";
    let us = word bytes 0 in
    if us < 1 || us > 100000 then invalid_arg "Playbit frame value";
    float us /. 1000000.

  let surface bytes =
    if Bytes.length bytes <> 16 then invalid_arg "Playbit surface length";
    let width = word bytes 0 and height = word bytes 4 in
    let pixel_scale = Int32.float_of_bits (Bytes.get_int32_le bytes 8) in
    let inputs = word bytes 12 in
    if
      width < 0 || height < 0
      || (not (Float.is_finite pixel_scale))
      || pixel_scale <= 0. || inputs < 0
      || inputs land lnot 7 <> 0
    then invalid_arg "Playbit surface value";
    { width; height; pixel_scale; inputs }
end

module View = struct
  let word out value =
    for shift = 0 to 3 do
      Buffer.add_char out
        (Char.chr
           (Int32.to_int
              (Int32.logand (Int32.shift_right_logical value (shift * 8)) 255l)))
    done

  let uint out value = word out (Int32.of_int value)

  let command_header out kind id length =
    if length > max_call_bytes - 9 - Buffer.length out then
      invalid_arg "Playbit scene exceeds limit";
    Buffer.add_char out (Char.chr kind);
    word out id;
    uint out length

  let command out kind id payload =
    command_header out kind id (String.length payload);
    Buffer.add_string out payload

  let create () =
    let out = Buffer.create 4096 in
    Buffer.add_string out view_magic;
    out

  let column_start out = command out view_column_start 0l ""
  let column_end out = command out view_column_end 0l ""
  let row_start out = command out view_row_start 0l ""
  let row_end out = command out view_row_end 0l ""

  let theme out overrides =
    let seen = ref 0 in
    List.iter (fun (role, _) ->
      if role < 0 || role >= ThemeColor.count || !seen land (1 lsl role) <> 0 then
        invalid_arg "Playbit theme color role";
      seen := !seen lor (1 lsl role)
    ) overrides;
    command_header out view_theme 0l (5 * List.length overrides);
    List.iter (fun (role, color) ->
      Buffer.add_char out (Char.chr role);
      word out color
    ) overrides

  let window_style out style =
    let appearance = WindowStyle.(theme_bright lor theme_dark) in
    if style land lnot WindowStyle.mask <> 0 || style land appearance = appearance then
      invalid_arg "Playbit window style";
    command_header out view_window_style 0l 4;
    uint out style

  let window_size_constraints ?(min_width=0.) ?(min_height=0.) ?(max_width=0.)
      ?(max_height=0.) ?(aspect_ratio=0.) out =
    (* Validate at wire precision. Underflow must not clear a requested limit. *)
    let binary32 x = Int32.float_of_bits (Int32.bits_of_float x) in
    let field x =
      if x = 0. then 0.
      else
        let value = binary32 x in
        if not (Float.is_finite value && value > 0.) then
          invalid_arg "Playbit window constraint value";
        value
    in
    let w_min = field min_width and h_min = field min_height in
    let w_max = field max_width and h_max = field max_height in
    let ratio = field aspect_ratio in
    let edge_limit = float_of_int max_window_canvas_edge in
    let largest edge = if edge = 0. then edge_limit else edge in
    let valid_axis lower upper = lower <= largest upper && upper <= edge_limit in
    if not (valid_axis w_min w_max && valid_axis h_min h_max) then
      invalid_arg "Playbit window size bounds";
    if ratio > 0. &&
        ((w_min > 0. && ratio < binary32 (w_min /. largest h_max))
         || (h_min > 0. && ratio > binary32 (largest w_max /. h_min))) then
      invalid_arg "Playbit window aspect ratio";
    command_header out view_window_size_constraints 0l 20;
    List.iter (fun value -> word out (Int32.bits_of_float value))
      [w_min; h_min; w_max; h_max; ratio]

  let text out style text =
    command out view_text 0l (String.make 1 (Char.chr style) ^ text)

  let button out id style label =
    command out view_button id (String.make 1 (Char.chr style) ^ "\000" ^ label)

  let context_menu out actions child =
    if actions = [] then invalid_arg "Playbit context menu requires actions";
    let length =
      List.fold_left
        (fun length (id, label) ->
          if id = 0l then invalid_arg "Playbit context menu action ID";
          let bytes = String.length label in
          if bytes > max_call_bytes - 11 - length then
            invalid_arg "Playbit context menu exceeds scene limit";
          length + 11 + bytes)
        4 actions
    in
    command_header out view_context_menu_start 0l length;
    uint out (List.length actions);
    List.iter
      (fun (id, label) ->
        Buffer.add_char out (Char.chr menu_action);
        word out id;
        Buffer.add_string out "\000\000";
        uint out (String.length label);
        Buffer.add_string out label)
      actions;
    child ();
    command out view_context_menu_end 0l ""

  let finish out = Buffer.contents out

  type rect = { x : int; y : int; width : int; height : int; color : int32 }

  type item =
    | Rect of rect
    | Text of rect * int * bool * string
    | Pixels of rect * int * int * bytes

  let rect out kind bounds =
    uint out kind;
    List.iter (uint out) [ bounds.x; bounds.y; bounds.width; bounds.height ];
    word out bounds.color

  let canvas_payload_length width height items =
    let text_bytes = ref 0 and pixel_bytes = ref 0 in
    List.fold_left
      (fun length item ->
        let bounds =
          match item with
          | Rect r | Text (r, _, _, _) | Pixels (r, _, _, _) -> r
        in
        if
          bounds.x < 0 || bounds.y < 0 || bounds.width < 1 || bounds.height < 1
          || bounds.x > width - bounds.width
          || bounds.y > height - bounds.height
        then invalid_arg "Playbit canvas item outside bounds";
        length + 24
        + match item with
        | Rect _ -> 0
        | Text (_, size, _, text) ->
            text_bytes := !text_bytes + String.length text;
            if
              size < 1
              || size > max_canvas_font_size
              || String.length text > 4096
              || !text_bytes > max_canvas_text_bytes
            then invalid_arg "Playbit canvas text limit";
            12 + String.length text
        | Pixels (_, w, h, bytes) ->
            pixel_bytes := !pixel_bytes + Bytes.length bytes;
            if
              w < 1 || h < 1 || w > max_canvas_edge || h > max_canvas_edge
              || w > max_canvas_pixel_bytes / 4 / h
              || Bytes.length bytes <> w * h * 4
              || !pixel_bytes > max_canvas_pixel_bytes
            then invalid_arg "Playbit pixel buffer limit";
            12 + Bytes.length bytes)
      16 items

  let canvas_item out = function
    | Rect r -> rect out 0 r
    | Text (r, size, bold, text) ->
        rect out 1 r;
        List.iter (uint out)
          [ size; (if bold then 1 else 0); String.length text ];
        Buffer.add_string out text
    | Pixels (r, w, h, bytes) ->
        rect out 2 { r with color = 0l };
        List.iter (uint out) [ w; h; Bytes.length bytes ];
        Buffer.add_bytes out bytes

  type canvas_extent = Inline | Viewport | Window

  let canvas_with_options ?clear_color ~extent out id ~width ~height ~continuous items =
    let max_edge = if extent = Window then max_window_canvas_edge else max_canvas_edge in
    if
      width < 1 || height < 1 || width > max_edge || height > max_edge
      || (id = 0l && continuous)
      || List.length items > max_canvas_items
    then invalid_arg "Playbit canvas dimensions";
    (* Validate and measure first, then write directly into the scene. A separate
       pixel-sized payload Buffer and Buffer.contents copy exceed small heaps. *)
    let length = canvas_payload_length width height items + (if Option.is_some clear_color then 4 else 0) in
    command_header out view_canvas id length;
    let flags =
      (if continuous then canvas_flag_continuous else 0)
      lor (match extent with Inline -> 0 | Viewport -> canvas_flag_viewport | Window -> canvas_flag_window)
      lor (if Option.is_some clear_color then canvas_flag_clear else 0)
    in
    List.iter (uint out) [ width; height; flags; List.length items ];
    Option.iter (word out) clear_color;
    List.iter (canvas_item out) items

  let canvas out id ~width ~height ~continuous items =
    canvas_with_options ~extent:Inline out id ~width ~height ~continuous items

  let viewport_canvas out id ~width ~height ~continuous items =
    canvas_with_options ~extent:Viewport out id ~width ~height ~continuous items

end

(* Private borrowed-call transport. It never exposes pointers to applications.
   Every typed adapter owns its records and copies outputs before returning. *)
module Borrowed = struct
  external call_raw : int32 -> bytes -> int64 = "caml_pb_call_packet"

  let create size =
    if size < 0 || size > max_call_bytes - 48 then
      invalid_arg "Playbit call exceeds limit";
    Bytes.make (size + 48) '\000'

  let pointer offset = Int64.of_int (80 + offset)

  let word packet offset value =
    Bytes.set_int32_le packet (48 + offset) (Int32.of_int value)

  let wide packet offset value = Bytes.set_int64_le packet (48 + offset) value
  let slot packet index value = Bytes.set_int64_le packet (index * 8) value

  let invoke opcode packet slots =
    if List.length slots > 6 then invalid_arg "Playbit call argument count";
    List.iteri (slot packet) slots;
    call_raw (Int32.of_int opcode) packet

  let call opcode packet slots = check (invoke opcode packet slots)

  let begin_handle opcode packet slots =
    let result = invoke opcode packet slots in
    check result;
    if result = 0L then raise (Host_error (Int32.of_int Error.internal));
    result

  let get_word packet offset =
    Int32.to_int (Bytes.get_int32_le packet (48 + offset))

  let get_wide packet offset = Bytes.get_int64_le packet (48 + offset)

  let put packet offset bytes =
    Bytes.blit bytes 0 packet (48 + offset) (Bytes.length bytes)

  let get packet offset length = Bytes.sub packet (48 + offset) length

  let record ?minimum packet offset size =
    let required = match minimum with Some value -> value | None -> size in
    let written = get_word packet (offset + 4) in
    if get_word packet offset <> size || written < required || written > size
    then failwith "Playbit incomplete output record"
end

let random_fill bytes =
  let length = Bytes.length bytes in
  if length > max_random_bytes then
    raise (Host_error (Int32.of_int Error.limit_exceeded));
  let packet = Borrowed.create length in
  Borrowed.call Opcode.random_fill packet
    [ Borrowed.pointer 0; Int64.of_int length ];
  Bytes.blit packet 48 bytes 0 length

module Session = struct
  type channel = int64
  type operation = int64
  type completion = Pending | Ready
  type info = { internal_id : string; key_secret : string option }

  let info () =
    let open Borrowed in
    let packet = create 64 in
    word packet 0 64;
    let result = invoke Opcode.session_info packet [pointer 0] in
    if result = Int64.of_int Error.not_found then None
    else (
      check result;
      record packet 0 64;
      let flags_and_reserved = get_wide packet 8 in
      let has_secret = Int64.logand flags_and_reserved 1L <> 0L in
      let secret = get packet 48 16 |> Bytes.unsafe_to_string in
      if result <> 0L || Int64.shift_right_logical flags_and_reserved 32 <> 0L
         || ((not has_secret) && secret <> String.make 16 '\000')
      then raise (Host_error (Int32.of_int Error.internal));
      Some { internal_id = get packet 16 32 |> Bytes.unsafe_to_string;
             key_secret = if has_secret then Some secret else None })

  let code info =
    Option.map (fun secret ->
        let alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz" in
        let digits = Bytes.make 22 '\000' in
        String.iter (fun byte ->
            let carry = ref (Char.code byte) in
            for i = Bytes.length digits - 1 downto 0 do
              let value = Bytes.get_uint8 digits i * 256 + !carry in
              Bytes.set_uint8 digits i (value mod 58);
              carry := value / 58
            done) secret;
        String.init 22 (fun i -> alphabet.[Bytes.get_uint8 digits i])) info.key_secret

  let identity () =
    let open Borrowed in
    let packet = create 104 in
    word packet 0 104;
    call Opcode.identity packet [ pointer 0 ];
    record packet 0 104;
    if Int64.logand (get_wide packet 96) (Int64.lognot 3L) <> 0L then
      raise (Host_error (Int32.of_int Error.internal));
    get packet 16 48 |> Bytes.unsafe_to_string

  let same_instance a b =
    String.length a = 48 && String.length b = 48
    && String.sub a 0 16 = String.sub b 0 16
    && String.sub a 32 16 = String.sub b 32 16

  let presence () =
    let open Borrowed in
    let participant_bytes = 88 in
    let read capacity =
      let packet = create (4 + capacity * participant_bytes) in
      for i = 0 to capacity - 1 do
        word packet (4 + i * participant_bytes) participant_bytes
      done;
      let result =
        invoke Opcode.presence packet
          [ pointer 4; Int64.of_int capacity; Int64.of_int participant_bytes;
            pointer 0 ]
      in
      (packet, result, get_word packet 0)
    in
    let packet, result, count = read 16 in
    let packet, result, count, capacity =
      if result = Int64.of_int Error.buffer_too_small && count > 16 && count <= 1024
      then
        let packet, result, actual = read count in
        (packet, result, actual, count)
      else (packet, result, count, 16)
    in
    check result;
    if count < 0 || count > capacity then failwith "Playbit presence count";
    List.init count (fun i ->
        let offset = 4 + i * participant_bytes in
        record packet offset participant_bytes;
        get packet (offset + 8) 48 |> Bytes.unsafe_to_string)

  let open_channel name =
    let open Borrowed in
    let length = String.length name in
    if length = 0 || length > 223 then invalid_arg "Playbit channel name length";
    call Opcode.session (create 0) [];
    let packet = create length in
    put packet 0 (Bytes.unsafe_of_string name);
    begin_handle Opcode.channel_open packet [ pointer 0; Int64.of_int length ]

  let control channel action =
    Borrowed.begin_handle Opcode.channel_begin (Borrowed.create 0) [ channel; action ]

  let subscribe channel = control channel 2L
  let unsubscribe channel = control channel 3L
  let close channel = control channel 4L

  let publish channel data =
    let open Borrowed in
    let length = String.length data in
    if length > 32768 then raise (Host_error (Int32.of_int Error.limit_exceeded));
    let packet = create length in
    put packet 0 (Bytes.unsafe_of_string data);
    begin_handle Opcode.channel_begin packet
      [ channel; 1L; pointer 0; Int64.of_int length ]

  let take operation =
    let open Borrowed in
    let result = invoke Opcode.channel_take (create 0) [ operation ] in
    if result = Int64.of_int Error.would_block then Pending
    else (check result; Ready)

  let event_handle data =
    let handle = Bytes.get_int64_le data 0 in
    if handle > 0L && Int64.to_int32 handle <> 0l
       && Int64.shift_right_logical handle 32 <> 0L
    then Some handle else None

  let ready event =
    if event.kind = Int32.of_int EventKind.data_ready && event.target = 0l
       && Bytes.length event.data = 8
    then event_handle event.data
    else None

  let message event =
    if event.kind = Int32.of_int EventKind.channel_message && event.target = 0l
       && Bytes.length event.data >= 104
    then Option.map (fun channel ->
      ( channel,
        Bytes.sub_string event.data 8 48,
        Bytes.sub_string event.data 104 (Bytes.length event.data - 104) )
    ) (event_handle event.data)
    else None

  let status event =
    if event.kind = Int32.of_int EventKind.channel_status && event.target = 0l
       && Bytes.length event.data = 4
    then
      let code = Bytes.get_int32_le event.data 0 in
      if code >= Int32.of_int Error.uncertain && code <= Int32.of_int Error.not_found
      then Some code else None
    else None
end

let wake_after milliseconds =
  Borrowed.call Opcode.wake_after (Borrowed.create 0) [ Int64.of_int milliseconds ]

let monotonic_ns () = Borrowed.invoke Opcode.clock_read (Borrowed.create 0) []

module Data = struct
  type volume = int64
  type volume_info = { volume : volume; can_write : bool }
  type volume_role = Root | Session | User | User_session | Local | Local_session
  type entry = bytes
  type operation = int64
  type 'a completion = Pending | Ready of 'a

  let role_tag = function
    | Root -> 1 | Session -> 2 | User -> 3
    | User_session -> 4 | Local -> 5 | Local_session -> 6

  let select_volume requested =
    let open Borrowed in
    let probe = create 64 in
    word probe 0 64;
    word probe 28 88;
    word probe 44 16;
    let result = invoke Opcode.available_volumes probe [ pointer 0 ] in
    if result <> Int64.of_int Error.buffer_too_small then check result;
    let count = get_word probe 24 and tags = get_word probe 40 in
    let names = get_word probe 56 in
    if
      count < 0 || count > 256 || tags < 0 || tags > 16384 || names < 0
      || names > max_data_bytes
    then failwith "Playbit volume capacities";
    let volume_at = 64 and tags_at = 64 + (count * 88) in
    let text_at = tags_at + (tags * 16) in
    if names > max_data_bytes - 48 - text_at then
      raise (Host_error (Int32.of_int Error.limit_exceeded));
    let p = create (text_at + names) in
    word p 0 64;
    List.iter
      (fun (at, v) -> word p at v)
      [
        (16, 80 + volume_at);
        (20, count);
        (28, 88);
        (32, 80 + tags_at);
        (36, tags);
        (44, 16);
        (48, 80 + text_at);
        (52, names);
      ];
    for index = 0 to count - 1 do
      word p (volume_at + (index * 88)) 88
    done;
    for index = 0 to tags - 1 do
      word p (tags_at + (index * 16)) 16
    done;
    call Opcode.available_volumes p [ pointer 0 ];
    record ~minimum:60 p 0 64;
    let returned = get_word p 24 in
    if returned < 0 || returned > count then failwith "Playbit volume count";
    (* Enumeration lends File aliases as well as volume identities. Release all
       aliases before selecting the root, including when no root is present. *)
    for index = 0 to returned - 1 do
      let at = volume_at + (index * 88) in
      record ~minimum:64 p at 88;
      let file = get_wide p (at + 56) in
      if file <> 0L then call Opcode.file_dispose (create 0) [ file ]
    done;
    let rec find index =
      if index = returned then raise (Host_error (Int32.of_int Error.not_found));
      let at = volume_at + (index * 88) in
      let selected =
        if requested = 0 then get_word p (at + 52) land 1 <> 0
        else
          let role = if get_word p (at + 4) >= 84 then get_word p (at + 80) else 0 in
          if role < 0 || role > 6 then failwith "Playbit volume role";
          role = requested
      in
      if selected then
        { volume = get_wide p (at + 8); can_write = get_word p (at + 48) land 2 <> 0 }
      else find (index + 1)
    in
    find 0

  let volume_info role = select_volume (role_tag role)
  let volume role = (volume_info role).volume
  let root_volume () = (select_volume 0).volume

  let resolve volume name =
    if name = "" || String.length name > 4096 then
      invalid_arg "Playbit entry name";
    let open Borrowed in
    let p = create (String.length name) in
    put p 0 (Bytes.of_string name);
    begin_handle Opcode.entry_resolve_begin p
      [ volume; pointer 0; Int64.of_int (String.length name) ]

  let take_entry operation =
    let open Borrowed in
    let p = create 32 in
    word p 0 32;
    let result = invoke Opcode.entry_resolve_take p [ operation; pointer 0 ] in
    if result = Int64.of_int Error.would_block then Pending
    else (
      check result;
      record p 0 32;
      Ready (get p 0 32))

  let entry p bytes =
    if Bytes.length bytes <> 32 then invalid_arg "Playbit entry handle";
    Borrowed.put p 0 bytes

  type key_cursor = { level : int; digest : bytes; after : string }

  let keys ?cursor handle =
    let open Borrowed in
    let query = 32 and after_at = 88 in
    let length = match cursor with None -> 0 | Some c -> String.length c.after in
    let p = create (after_at + length) in
    entry p handle;
    word p query 56;
    word p (query + 48) 64;
    Option.iter (fun c ->
      word p (query + 8) c.level;
      put p (query + 12) c.digest;
      word p (query + 44) 1;
      put p after_at (Bytes.of_string c.after)) cursor;
    begin_handle Opcode.kv_keys_begin p
      [pointer 0; pointer query; (if length = 0 then 0L else pointer after_at);
       Int64.of_int length]

  let take_keys operation =
    let open Borrowed in
    let capacity = 40 + 64 * (10 + 511) + 511 in
    let p = create (8 + capacity) in
    let result = invoke Opcode.kv_keys_take p
      [operation; pointer 8; Int64.of_int capacity; pointer 0] in
    if result = Int64.of_int Error.would_block then Pending
    else (
      check result;
      let size = get_word p 0 in
      let invalid () = failwith "Invalid Playbit KV key page" in
      if size < 40 || size > capacity then invalid ();
      let page = get p 8 size in
      let present = Bytes.get_uint8 page 0 and count = Bytes.get_int32_le page 34 in
      let next_length = Bytes.get_uint16_le page 38 in
      if present > 1 || count < 0l || count > 64l || next_length > 511 ||
         (present = 0 && (count <> 0l || next_length <> 0)) then invalid ();
      let rec read at left found =
        if left = 0 then (
          if at + next_length <> size then invalid ();
          let next = if next_length = 0 then None else (
            let after = Bytes.sub_string page at next_length in
            (match found with last :: _ when last > after -> invalid () | _ -> ());
            Some {level = Bytes.get_uint8 page 1; digest = Bytes.sub page 2 32; after}) in
          Ready (List.rev found, next))
        else (
          if at + 10 > size then invalid ();
          let length = Bytes.get_uint16_le page at in
          if length = 0 || length > 511 || at + 10 + length > size then invalid ();
          let key = Bytes.sub_string page (at + 10) length in
          (match found with last :: _ when last >= key -> invalid () | _ -> ());
          read (at + 10 + length) (left - 1) (key :: found))
      in
      read 40 (Int32.to_int count) [])

  let read handle key ~limit =
    if key = "" || String.length key > 511 || limit < 1 || limit > 61440 then
      invalid_arg "Playbit KV read arguments";
    let open Borrowed in
    let query = 32 and key_at = 96 in
    let p = create (key_at + String.length key) in
    entry p handle;
    word p query 64;
    word p (query + 16) limit;
    put p key_at (Bytes.of_string key);
    Borrowed.begin_handle Opcode.kv_read_begin p
      [
        pointer 0;
        pointer key_at;
        Int64.of_int (String.length key);
        pointer query;
      ]

  let take_read operation ~limit =
    if limit < 1 || limit > 61440 then invalid_arg "Playbit KV read limit";
    let open Borrowed in
    let capacity = 49 + limit in
    let p = create (8 + capacity) in
    let result =
      invoke Opcode.kv_read_take p
        [ operation; pointer 8; Int64.of_int capacity; pointer 0 ]
    in
    if result = Int64.of_int Error.would_block then Pending
    else (
      check result;
      let count = get_word p 0 in
      if count < 49 || count > capacity then failwith "Playbit KV chunk length";
      let total = get_wide p (8 + 33) and offset = get_wide p (8 + 41) in
      if offset <> 0L || total <> Int64.of_int (count - 49) then
        failwith "Playbit KV value exceeds requested limit";
      Ready (get p (8 + 49) (count - 49)))

  let put handle key value =
    if key = "" || String.length key > 511 || Bytes.length value > 61440 then
      invalid_arg "Playbit KV put arguments";
    let open Borrowed in
    let key_at = 32 in
    let value_at = key_at + String.length key in
    let p = create (value_at + Bytes.length value) in
    entry p handle;
    put p key_at (Bytes.of_string key);
    put p value_at value;
    Borrowed.begin_handle Opcode.kv_put_begin p
      [
        pointer 0;
        pointer key_at;
        Int64.of_int (String.length key);
        pointer value_at;
        Int64.of_int (Bytes.length value);
      ]

  let take_put operation =
    let p = Borrowed.create 0 in
    let result = Borrowed.invoke Opcode.kv_put_take p [ operation ] in
    if result = Int64.of_int Error.would_block then Pending
    else (
      check result;
      Ready ())

  let dispose operation =
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.operation_dispose p [ operation ]
end

module Audio = struct
  type t = int64
  type operation = int64
  type 'a completion = Pending | Ready of 'a

  let open_begin ~preferred_sample_rate =
    if preferred_sample_rate <> 0
       && (preferred_sample_rate < 8000 || preferred_sample_rate > 192000)
    then invalid_arg "Playbit preferred audio sample rate";
    let open Borrowed in
    let p = create 0 in
    begin_handle Opcode.audio_open_begin p [ Int64.of_int preferred_sample_rate ]

  let open_take operation =
    let open Borrowed in
    let p = create 0 in
    let result = invoke Opcode.audio_open_take p [ operation ] in
    if result = Int64.of_int Error.would_block then Pending
    else (check result;
      if result = 0L then raise (Host_error (Int32.of_int Error.internal));
      Ready result)

  let dispose operation =
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.operation_dispose p [ operation ]

  let close audio =
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.audio_close p [ audio ]

  let sample_rate audio =
    let p = Borrowed.create 0 in
    let result = Borrowed.invoke Opcode.audio_sample_rate p [ audio ] in
    Int32.to_int (scalar32 result)

  let volume audio =
    let p = Borrowed.create 0 in
    let result = Borrowed.invoke Opcode.audio_volume p [ audio ] in
    Int32.float_of_bits (scalar32 result)

  let set_volume audio volume =
    if (not (Float.is_finite volume)) || volume < 0. || volume > 1. then
      invalid_arg "Playbit audio volume";
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.audio_set_volume p
      [ audio; Int64.logand (Int64.of_int32 (Int32.bits_of_float volume)) 0xffffffffL ]
end

module Sound = struct
  type t = int64
  type instance = { sound : t; id : int32 }
  let loop = 1l

  let unsigned value = Int64.logand (Int64.of_int32 value) 0xffffffffL

  let open_pcm audio ~channel_count ~sample_rate samples =
    if channel_count < 1 || channel_count > 2
       || sample_rate < 8000 || sample_rate > 192000
       || Bytes.length samples = 0
       || Bytes.length samples mod (channel_count * 2) <> 0
    then invalid_arg "Playbit PCM format or length";
    let open Borrowed in
    let p = create (Bytes.length samples) in
    put p 0 samples;
    begin_handle Opcode.sound_open_pcm p
      [ audio; Int64.of_int channel_count; Int64.of_int sample_rate;
        pointer 0; Int64.of_int (Bytes.length samples / (channel_count * 2)) ]

  let close sound =
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.sound_close p [ sound ]

  let instance sound id = { sound; id }

  let gain_word gain =
    if (not (Float.is_finite gain)) || gain < 0. || gain > 1. then
      invalid_arg "Playbit sound gain";
    Int64.of_int (int_of_float ((gain *. 65536.) +. 0.5))

  let play sound ~gain ~start ~length ~flags =
    if length = 0L || Int32.logand flags (Int32.lognot loop) <> 0l then
      invalid_arg "Playbit sound playback segment or flags";
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.sound_play p
      [ sound.sound; unsigned sound.id; gain_word gain; start; length; unsigned flags ]

  let tell sound =
    let open Borrowed in
    let p = create 8 in
    call Opcode.sound_tell p [ sound.sound; unsigned sound.id; pointer 0 ];
    get_wide p 0

  let seek sound position =
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.sound_seek p [ sound.sound; unsigned sound.id; position ]

  let set_gain sound gain =
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.sound_set_gain p [ sound.sound; unsigned sound.id; gain_word gain ]

  let set_balance sound balance =
    if (not (Float.is_finite balance)) || balance < -1. || balance > 1. then
      invalid_arg "Playbit sound balance";
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.sound_set_balance p
      [ sound.sound; unsigned sound.id; unsigned (Int32.bits_of_float balance) ]

  let stop sound =
    let p = Borrowed.create 0 in
    Borrowed.call Opcode.sound_stop p [ sound.sound; unsigned sound.id ]
end

let run turn = Callback.register "playbit.turn" turn
