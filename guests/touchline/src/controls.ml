(* Keyboard directions follow the screen, even though the pitch is tilted.
   Clicking or dragging gives the player a destination in pitch coordinates. *)
type t = {
  arrows : bool array;
  mutable destination : (float * float) option;
  mutable space : float option;
  mutable pass : bool;
  mutable shoot : bool;
}

let create () =
  { arrows = Array.make 4 false; destination = None; space = None;
    pass = false; shoot = false }

let clear t =
  Array.fill t.arrows 0 4 false;
  t.destination <- None;
  t.space <- None;
  t.pass <- false;
  t.shoot <- false

let key t code pressed =
  match code with
  | 1 | 2 | 3 | 4 ->
      t.arrows.(code - 1) <- pressed;
      if pressed then t.destination <- None
  | 5 ->
      (match pressed, t.space with
       | true, None -> t.space <- Some 0.
       | false, Some held ->
           if held < 0.23 then t.pass <- true;
           t.space <- None
       | _ -> ())
  | _ -> ()

let direction t (player : Match.player) =
  match t.destination with
  | Some (x, y) ->
      let dx = x -. player.x and dy = y -. player.y in
      if dx *. dx +. dy *. dy < 0.49 then (t.destination <- None; 0., 0.)
      else dx, dy
  | None ->
      let axis positive negative =
        float (Bool.to_int t.arrows.(positive) - Bool.to_int t.arrows.(negative))
      in
      let x, y = Art.unproject ~x:(axis 1 0) ~y:(axis 3 2) in
      let ox, oy = Art.unproject ~x:0. ~y:0. in
      x -. ox, y -. oy

let frame t dt player =
  (match t.space with
   | Some held ->
       let next = held +. dt in
       if held < 0.23 && next >= 0.23 then t.shoot <- true;
       t.space <- Some next
   | None -> ());
  let dx, dy = match player with None -> 0., 0. | Some p -> direction t p in
  let length = sqrt (dx *. dx +. dy *. dy) in
  let dx, dy = if length = 0. then 0., 0. else dx /. length, dy /. length in
  let input = Match.{ dx; dy; pass = t.pass; shoot = t.shoot } in
  t.pass <- false;
  t.shoot <- false;
  input
