module Room = Session
open Playbit

let room = Room.create ()
let controls = Controls.create ()
let book = Scorebook.create ()
let canvas = 1l
let background = 0xe6ebdcffl
let window_style = WindowStyle.(standard lor transparent_titlebar lor hidden_title lor theme_bright)
let pass = 2l
let shoot = 3l
let rematch = 4l
let reconnect = 5l
let shirts = [| 1; 2; 3; 7; 8; 11; 9 |]
let club = function Match.Home -> "Portside FC" | Match.Away -> "Rovers"

let surface = ref Input.{ width = 720; height = 545; pixel_scale = 1.; inputs = 3 }
let layout = ref None
let pressing_hint = ref false
let positions = Array.make Match.player_count (0., 0.)
let dirty = ref true
let initialized = ref false

let connect () = Scorebook.open_session book (fun () -> Room.start room)

let current_player () =
  Option.map (fun slot -> (Room.game room).players.(slot)) (Room.local_slot room)

let click target =
  if target = reconnect then connect ()
  else if target = rematch && (Room.game room).phase = Match.Full_time then (
    Controls.clear controls;
    Room.restart room)
  else if Room.local_slot room <> None then
    if target = pass then controls.pass <- true
    else if target = shoot then controls.shoot <- true

let pointer (event : Input.window_pointer) =
  let x = int_of_float event.canvas_x and y = int_of_float event.canvas_y in
  Option.iter (fun (_, (view : Match_view.t)) ->
    let action = Option.bind view.action (fun (bounds, action) ->
      if Pixel_layout.contains bounds x y then Some action else None) in
    match event.phase, action with
    | 1, Some Match_view.Rematch -> click rematch
    | 1, Some Play -> pressing_hint := true; Controls.key controls 5 true
    | (2 | 3), _ when !pressing_hint ->
        pressing_hint := false;
        if event.phase = 3 || action <> Some Play then controls.space <- None
        else Controls.key controls 5 false
    | (0 | 1), _ when event.buttons land 1 <> 0 && not !pressing_hint
                       && (Room.game room).phase <> Match.Full_time ->
        Option.iter (fun fit ->
          if Pixel_layout.contains fit.Pixel_layout.box x y then (
            let x, y = Pixel_layout.to_source fit x y in
            let x, y = Art.unproject ~x ~y in
            if x >= 0. && x <= Match.width && y >= 0. && y <= Match.height then
              controls.destination <- Some (x, y))) view.stadium
    | _ -> ()) !layout

let update (e : event) =
  let is kind = e.kind = Int32.of_int kind in
  let old_game = Room.game room in
  let tick = old_game.tick and phase = old_game.phase in
  let status = Room.status room and slot = Room.local_slot room in
  Room.event room e;
  if is EventKind.surface_changed then (
    surface := Input.surface e.data;
    if !surface.width = 0 || !surface.height = 0 then (
      pressing_hint := false; Controls.clear controls);
    dirty := true)
  else if is EventKind.click then click e.target
  else if is EventKind.data_ready then
    Scorebook.complete book (fun () -> Room.start room)
  else if is EventKind.window_keyboard then (
    let k = Input.window_keyboard e.data in
    if k.device_key = Key.space && old_game.phase = Match.Full_time then (
      if k.pressed && not k.repeat && Room.can_restart room then click rematch)
    else
      let direction =
        if k.device_key = Key.left then 1
        else if k.device_key = Key.right then 2
        else if k.device_key = Key.up then 3
        else if k.device_key = Key.down then 4
        else if k.device_key = Key.space then 5
        else 0 in
      Controls.key controls direction k.pressed)
  else if e.target = canvas then (
    if is EventKind.window_pointer then (
      let input = Input.window_pointer e.data in
      if input.flags land 1 <> 0 then pointer input)
    else if is EventKind.frame then (
      let dt = Input.frame e.data in
      let input = Controls.frame controls dt (current_player ()) in
      Room.frame room dt input;
      if input.dx <> 0. || input.dy <> 0. then dirty := true));
  let game = Room.game room in
  (* The player running the simulation may only have read access. Each seated
     writer sees the same final whistle and records the same score. *)
  if game.phase = Match.Full_time && phase <> Match.Full_time && Room.status room = Room.Online then
    Scorebook.full_time book game;
  let next_slot = Room.local_slot room in
  if Option.map Match.team_of_slot slot <> Option.map Match.team_of_slot next_slot then (
    pressing_hint := false;
    Controls.clear controls);
  if tick <> game.tick || game != old_game || status <> Room.status room
     || slot <> next_slot || is EventKind.presence_changed
     || is EventKind.click || is EventKind.data_ready && book.notice <> None then
    dirty := true

let sprites (game : Match.t) =
  let slot = Room.local_slot room in
  Array.map (fun (p : Match.player) ->
    let x, y = positions.(p.slot) in
    let px, py = Room.display_position room p.slot in
    let moving = abs_float (px -. x) +. abs_float (py -. y) > 0.05 in
    positions.(p.slot) <- (px, py);
    Art.{ x = px; y = py; home = p.team = Match.Home; keeper = Match.keeper p.slot;
          selected = slot = Some p.slot; moving; facing_x = p.facing_x;
          facing_y = p.facing_y; number = shirts.(Match.role p.slot) }
  ) game.players

(* The ball stays at the local dribbler's feet between network snapshots. *)
let ball_position (game : Match.t) =
  match game.ball.owner with
  | Some slot when Room.local_slot room = Some slot ->
      let x, y = Room.display_position room slot in
      let player = game.players.(slot) in
      game.ball.x +. (x -. player.x), game.ball.y +. (y -. player.y)
  | _ -> game.ball.x, game.ball.y

let has_ball (game : Match.t) =
  match Room.local_slot room with Some slot -> game.ball.owner = Some slot | None -> false

let footer game status =
  let team_label = match current_player () with
    | Some player -> "You play for " ^ club player.team
    | None -> "Spectating" in
  let count = Room.human_count room in
  let participants = string_of_int count ^ (if count = 1 then " participant" else " participants") in
  let keyboard = !surface.inputs land 2 <> 0 in
  let hint, action = match status, game.Match.phase with
    | Room.Full, _ | _, Match.Full_time -> "", None
    | _ when has_ball game ->
        (if keyboard then "SPACE - tap to pass, hold to shoot" else "Tap to pass, hold to shoot"),
        Some Match_view.Play
    | _ ->
        (if keyboard then "SPACE - tackle or switch player" else "Tap to tackle or switch player"),
        Some Play in
  let message = match status, game.phase with
    | Room.Full, _ -> Some "Game is full. Start a new game session"
    | Room.Offline, _ -> Some "Offline practice"
    | Room.Connecting, _ -> Some "Joining session..."
    | _, Match.Goal team -> Some ("Goal! " ^ club team)
    | _ -> None in
  let message = match book.notice with
    | Some notice when status <> Room.Full -> Some notice
    | _ -> message in
  Match_view.{club = team_label; participants; hint; action; message}

let render () =
  let game = Room.game room and status = Room.status room in
  let full_time =
    if game.phase <> Match.Full_time then None
    else
      let replay = if not (Room.can_restart room) then Match_view.Waiting
        else if !surface.inputs land 2 <> 0 then Keyboard else Touch in
      Some Match_view.{ home_score = game.home_score; away_score = game.away_score; replay } in
  let footer = footer game status in
  let key = (!surface, footer, full_time) in
  let view = match !layout with
    | Some (old, view) when old = key -> view
    | _ ->
        let view = Match_view.create ?full_time ~width:!surface.width ~height:!surface.height
          ~density:!surface.pixel_scale footer in
        layout := Some (key, view);
        view in
  let pitch () =
    let ball_x, ball_y = ball_position game in
    Art.draw ~players:(sprites game) ~ball_x ~ball_y ~ball_z:0. ~ticks:game.tick in
  let board () =
    let seconds = (game.ticks_left + Match.ticks_per_second - 1) / Match.ticks_per_second in
    Art.draw_scorebug ~home_score:game.home_score ~away_score:game.away_score ~seconds in
  let scene = View.create () in
  View.window_input scene (Input.pointers lor Input.keyboard);
  View.theme scene [ThemeColor.window_background, background];
  View.window_style scene window_style;
  let canvas () = View.canvas_with_options ~extent:View.Window ~clear_color:background
    scene canvas ~width:view.width ~height:view.height
    ~continuous:true (Match_view.draw view ~pitch ~board) in
  let actions = match status, game.phase with
    | Room.Full, _ -> []
    | _, Match.Full_time when Room.can_restart room -> [rematch, "Kick off again"]
    | _, Match.Full_time -> []
    | _ when has_ball game -> [pass, "Pass"; shoot, "Shoot"]
    | _ -> [pass, "Tackle or switch player"] in
  let actions = if status = Room.Offline then (reconnect, "Reconnect") :: actions else actions in
  (if actions = [] then canvas () else View.context_menu scene actions canvas);
  submit (View.finish scene);
  dirty := false

let turn () =
  if not !initialized then (initialized := true; connect ());
  let rec drain remaining =
    if remaining > 0 then
      match poll () with
      | None -> ()
      | Some event -> update event; drain (remaining - 1)
  in
  drain max_queued_events;
  if !dirty then render ()

let () =
  quit_on_last_window true;
  run turn
