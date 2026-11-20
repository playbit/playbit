open Pixel_layout

type action = Play | Rematch
type footer = {
  club : string;
  participants : string;
  hint : string;
  action : action option;
  message : string option;
}
type replay = Keyboard | Touch | Waiting
type full_time = { home_score : int; away_score : int; replay : replay }
type t = {
  width : int;
  height : int;
  stadium : fit option;
  scorebug : fit option;
  labels : Playbit.View.item list;
  result : Playbit.View.item option;
  action : (box * action) option;
}

let item fit image =
  let b = fit.box in
  Playbit.View.Pixels
    (Playbit.View.{x = b.x; y = b.y; width = b.w; height = b.h; color = 0l},
     fit.texels_w, fit.texels_h, pixels fit image)

let paint (image : image) ~x ~y ~w ~h color =
  for row = y to y + h - 1 do
    for col = x to x + w - 1 do
      Bytes.set_int32_be image.rgba (4 * (row * image.w + col)) color
    done
  done

let text image ~x ~y ~scale color label =
  Pixel_font.draw (fun col row ->
    paint image ~x:(x + col * scale) ~y:(y + row * scale) ~w:scale ~h:scale color
  ) ~x:0 ~y:0 label

let lettering label =
  let w = Pixel_font.measure label and h = Pixel_font.height in
  let image = { w; h; rgba = Bytes.make (4 * w * h) '\000' } in
  text image ~x:0 ~y:0 ~scale:1 0x1b221fffl label;
  image

let result_panel full_time =
  let title, accent =
    if full_time.home_score > full_time.away_score then "Portside FC won", 0xf27958ffl
    else if full_time.home_score < full_time.away_score then "Rovers won", 0x91acedffl
    else "Tie", 0xefda95ffl in
  let prompt = match full_time.replay with
    | Keyboard -> "Press SPACE to play again"
    | Touch -> "Tap to play again"
    | Waiting -> "Waiting for the next kickoff" in
  let title_w = 2 * Pixel_font.measure title and prompt_w = Pixel_font.measure prompt in
  let w = max 152 (max title_w (prompt_w + 10) + 24) and h = 59 in
  let image = { w; h; rgba = Bytes.make (4 * w * h) '\000' } in
  let panel_w = w - 3 in
  paint image ~x:3 ~y:3 ~w:panel_w ~h:(h - 3) 0x163a2bffl;
  paint image ~x:1 ~y:0 ~w:(panel_w - 2) ~h:(h - 3) 0x748f7dffl;
  paint image ~x:0 ~y:1 ~w:panel_w ~h:(h - 5) 0x1b2d28ffl;
  paint image ~x:1 ~y:(h - 4) ~w:(panel_w - 2) ~h:1 0x121f1affl;
  paint image ~x:(panel_w / 2 - 8) ~y:8 ~w:16 ~h:2 accent;
  text image ~x:((panel_w - title_w) / 2) ~y:14 ~scale:2 0xfff7daffl title;
  let prompt_x = (panel_w - prompt_w) / 2 in
  if full_time.replay <> Waiting then
    paint image ~x:(prompt_x - 5) ~y:37 ~w:(prompt_w + 10) ~h:14 0x33493effl;
  text image ~x:prompt_x ~y:38 ~scale:1 0xdce6cbffl prompt;
  image

let result_layer ~density ~font_zoom (pitch : fit) full_time =
  let image = result_panel full_time in
  let b = pitch.box in
  let inset = min 8 (min b.w b.h / 8) in
  let area = { x = b.x + inset; y = b.y + inset; w = b.w - 2 * inset; h = b.h - 2 * inset } in
  let max_zoom = max font_zoom pitch.zoom in
  Option.map (fun (panel : fit) ->
    let center_x, center_y = Art.project ~x:(Match.width /. 2.) ~y:(Match.height /. 2.) in
    let x = b.x + center_x * b.w / (pitch.texels_w * pitch.sample) - panel.box.w / 2 in
    let y = b.y + center_y * b.h / (pitch.texels_h * pitch.sample) - panel.box.h / 2 in
    let bounds = { panel.box with
      x = max area.x (min (area.x + area.w - panel.box.w) x);
      y = max area.y (min (area.y + area.h - panel.box.h) y) } in
    let action = if full_time.replay = Waiting then None else Some (bounds, Rematch) in
    (item { panel with box = bounds } image, action)
  ) (fit ~density ~max_zoom area ~w:image.w ~h:image.h)

let wrap text max_width =
  let add (lines, current) word =
    let joined = if current = "" then word else current ^ " " ^ word in
    if current = "" || Pixel_font.measure joined <= max_width then lines, joined
    else current :: lines, word
  in
  let lines, current = List.fold_left add ([], "") (String.split_on_char ' ' text) in
  List.rev (current :: lines)

let create ?full_time ~width ~height ~density footer =
  let width = max 1 (min Playbit.max_canvas_edge width)
  and height = max 1 (min Playbit.max_canvas_edge height) in
  let margin = min 8 (width / 32) in
  let usable = width - 2 * margin in
  let font_zoom = max 1 (int_of_float (Float.round density)) in
  let points px = int_of_float (ceil (float (px * font_zoom) /. density)) in
  let measure text = points (Pixel_font.measure text) in
  let line = points Pixel_font.height + 4 in
  let left = measure footer.club and center = measure footer.participants
  and right = measure footer.hint in
  let across = max left right + 8 <= (usable - center) / 2 in
  let pair = left + 8 + center <= usable in
  let rows =
    if across then [[`Left, footer.club; `Center, footer.participants; `Right, footer.hint]]
    else if pair then [[`Left, footer.club; `Right, footer.participants]; [`Center, footer.hint]]
    else [[`Center, footer.club]; [`Center, footer.participants]; [`Center, footer.hint]]
  in
  let rows = List.filter (List.exists (fun (_, text) -> text <> "")) rows in
  let footer_y = max 0 (height - List.length rows * line - min 4 (height / 100)) in
  let labels = ref [] and action = ref None in
  let label ~align ~y ~row_h ~interactive text =
    let image = lettering text in
    let area = { x = margin; y; w = usable; h = min row_h (height - y) } in
    match fit ~density ~max_zoom:font_zoom area ~w:image.w ~h:image.h with
    | None -> ()
    | Some f ->
        let x = match align with `Left -> margin | `Center -> f.box.x | `Right -> width - margin - f.box.w in
        let b = { f.box with x } in
        labels := item { f with box = b } image :: !labels;
        if interactive then
          action := Option.map (fun a ->
            ({ x = max 0 (x - 4); y; w = b.w + 8; h = row_h }, a)) footer.action
  in
  List.iteri (fun row labels ->
    List.iter (fun (align, text) ->
      label ~align ~y:(footer_y + row * line) ~row_h:line ~interactive:(text = footer.hint) text
    ) labels) rows;
  let top = min 8 (height / 45) in
  let messages = match footer.message with
    | None -> []
    | Some text -> wrap text (max 1 (int_of_float (float usable *. density /. float font_zoom)))
  in
  let board_area = { x = 0; y = top; w = width; h = max 0 ((footer_y - top) / 4) } in
  (* Leave room for a goal call so the pitch stays under the pointer when a team scores. *)
  let stadium_top score_h = top + score_h + max 1 (List.length messages) * line + min 8 (height / 40) in
  (* Size the ground and scoreboard together: on a big display both should grow. *)
  let pitch_room = { x = 0; y = 0; w = width; h = max 0 (footer_y - top) } in
  let max_zoom = max 1 (min (device density width / Art.width)
    (device density pitch_room.h / (Art.height + Art.scorebug_height))) in
  let scorebug =
    Option.map (fun f -> { f with box = { f.box with y = top } })
      (fit ~density ~max_zoom board_area ~w:Art.scorebug_width ~h:Art.scorebug_height) in
  let score_h = Option.fold ~none:0 ~some:(fun f -> f.box.h) scorebug in
  List.iteri (fun i message ->
    label ~align:`Center ~y:(top + score_h + i * line) ~row_h:line ~interactive:false message
  ) messages;
  let y = stadium_top score_h in
  let area = { x = 0; y; w = width; h = max 0 (footer_y - y - min 6 (height / 50)) } in
  let stadium = fit ~density ~max_zoom:max_int area ~w:Art.width ~h:Art.height in
  let result, action = match full_time, stadium with
    | Some full_time, Some pitch ->
        (match result_layer ~density ~font_zoom pitch full_time with
         | Some (result, action) -> (Some result, action)
         | None -> (None, None))
    | _ -> (None, !action) in
  { width; height; stadium; scorebug; labels = List.rev !labels; result; action }

let draw layout ~pitch ~board =
  let layer fit w h pixels = Option.to_list (Option.map (fun f ->
    item f { w; h; rgba = pixels () }) fit) in
  layer layout.stadium Art.width Art.height pitch
  @ layer layout.scorebug Art.scorebug_width Art.scorebug_height board
  @ Option.to_list layout.result
  @ layout.labels
