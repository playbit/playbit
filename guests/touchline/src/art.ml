let width = 506
let height = 294
let stride = width * 4
let length = stride * height

let nearest n = int_of_float (if n >= 0. then n +. 0.5 else n -. 0.5)

let origin_x = 142.
let origin_y = 62.

let project ~x ~y =
  (nearest (origin_x +. 3.15 *. x -. 1.77 *. y),
   nearest (origin_y +. 1.20 *. x +. 1.22 *. y))

let unproject ~x ~y =
  let u = x -. origin_x and v = y -. origin_y in
  let determinant = 3.15 *. 1.22 +. 1.77 *. 1.20 in
  ((1.22 *. u +. 1.77 *. v) /. determinant,
   (3.15 *. v -. 1.20 *. u) /. determinant)

let at x y z =
  let px, py = project ~x ~y in
  (px, py - z)

let rgba_write buf i r g b a =
  Bytes.unsafe_set buf i r;
  Bytes.unsafe_set buf (i + 1) g;
  Bytes.unsafe_set buf (i + 2) b;
  Bytes.unsafe_set buf (i + 3) a

let pixel buf x y color =
  if x >= 0 && x < width && y >= 0 && y < height then
    rgba_write buf (y * stride + x * 4)
      (Char.chr (color lsr 16)) (Char.chr (color lsr 8 land 255))
      (Char.chr (color land 255)) '\255'

let span ?(alpha = '\255') buf y x0 x1 color =
  if y >= 0 && y < height then (
    let r = Char.chr (color lsr 16) in
    let g = Char.chr (color lsr 8 land 255) in
    let b = Char.chr (color land 255) in
    let start = y * stride in
    for x = max 0 x0 to min (width - 1) x1 do
      rgba_write buf (start + x * 4) r g b alpha
    done)

let rect buf x y w h color =
  for row = y to y + h - 1 do
    span buf row x (x + w - 1) color
  done

let line buf (x0, y0) (x1, y1) color =
  let dx = abs (x1 - x0) and dy = -(abs (y1 - y0)) in
  let sx = if x0 < x1 then 1 else -1 in
  let sy = if y0 < y1 then 1 else -1 in
  let rec visit x y error =
    pixel buf x y color;
    if x <> x1 || y <> y1 then (
      let twice = 2 * error in
      let next_x, error = if twice >= dy then (x + sx, error + dy) else (x, error) in
      let next_y, error = if twice <= dx then (y + sy, error + dx) else (y, error) in
      visit next_x next_y error)
  in
  visit x0 y0 (dx + dy)

let polygon ?(alpha = '\255') buf color points =
  let points = Array.of_list points in
  let count = Array.length points in
  let top = Array.fold_left (fun n (_, y) -> min n y) height points in
  let bottom = Array.fold_left (fun n (_, y) -> max n y) 0 points in
  for y = max top 0 to min bottom (height - 1) do
    let left = ref max_int and right = ref min_int in
    for i = 0 to count - 1 do
      let x0, y0 = points.(i) and x1, y1 = points.((i + 1) mod count) in
      if y >= min y0 y1 && y <= max y0 y1 then (
        if y0 = y1 then (
          left := min !left (min x0 x1);
          right := max !right (max x0 x1))
        else (
          let x = x0 + (y - y0) * (x1 - x0) / (y1 - y0) in
          left := min !left x;
          right := max !right x))
    done;
    span ~alpha buf y !left !right color
  done

let plane buf color x0 y0 x1 y1 z =
  polygon buf color [at x0 y0 z; at x1 y0 z; at x1 y1 z; at x0 y1 z]

let edge buf color x0 y0 x1 y1 z0 z1 =
  polygon buf color [at x0 y0 z0; at x1 y1 z0; at x1 y1 z1; at x0 y0 z1]

let ellipse buf x y rx ry color =
  for row = -ry to ry do
    let ratio = float row /. float ry in
    let dx = Float.to_int (float rx *. sqrt (max 0. (1. -. ratio *. ratio))) in
    span buf (y + row) (x - dx) (x + dx) color
  done

let hash a b =
  let n = a * 196613 lxor (b * 131101) in
  let n = (n lxor (n lsr 11)) * 21929 in
  (n lxor (n lsr 13)) land 16383

let random_sample state =
  let n = (!state lxor (!state lsl 11)) land 0x1fffffff in
  let n = n lxor (n lsr 9) in
  let n = (n lxor (n lsl 7)) land 0x1fffffff in
  state := n;
  n

let random_int state limit = random_sample state mod limit
let random_float state limit = float (random_sample state) *. limit /. 536870912.

let ink = 0x1f2c27
let coral = 0xeb7357
let lapis = 0x6789dc
let white = 0xfff7da

let earth buf random =
  (* Layered concrete plinth. The lip and courses follow the same projection as the turf. *)
  let offset dx dy (x, y) = (x + dx, y + dy) in
  let corners = [at (-5.) (-12.) 0; at 105. (-12.) 0; at 105. 69. 0; at (-5.) 69. 0] in
  polygon ~alpha:'\016' buf 0x141b17 (List.map (offset 8 17) corners);
  polygon ~alpha:'\032' buf 0x141b17 (List.map (offset 5 14) corners);
  edge buf 0x826c58 (-5.) 69. 105. 69. 0 (-12);
  edge buf 0x65594c 105. (-12.) 105. 69. 0 (-12);
  edge buf 0x9e826a (-5.) 69. 105. 69. (-2) (-4);
  edge buf 0x9e826a (-5.) 69. 105. 69. (-8) (-9);
  edge buf 0x766453 105. (-12.) 105. 69. (-3) (-5);
  for i = 0 to 128 do
    let x = -5. +. random_float random 110. in
    let px, py = at x 69. (-5 - random_int random 7) in
    span buf py px (px + random_int random 3) (if i mod 3 = 0 then 0xaf9177 else 0x74604f)
  done;
  plane buf 0xcbb592 (-5.) (-12.) 105. 69. 0;
  plane buf 0xd9c4a0 (-4.7) (-11.7) 104.7 68.7 0;
  for i = 0 to 2400 do
    let x = -5. +. random_float random 110. in
    let y = -12. +. random_float random 81. in
    let px, py = at x y 0 in
    pixel buf px py (match i mod 4 with 0 -> 0xbda17f | 1 -> 0xc8ad8a | 2 -> 0xe1cba8 | _ -> 0xdbc5a4)
  done;
  edge buf 0xe5cda6 (-4.8) 68.8 104.8 68.8 0 1;
  edge buf 0x908361 103.9 (-11.) 103.9 68.2 0 1;
  edge buf 0x9e8b70 (-4.2) 67.2 103.9 67.2 0 1;
  for i = 0 to 16 do
    let x = -4. +. float i *. 6.5 in
    line buf (at x 65.7 0) (at x 68.6 0) 0xb29b7e
  done

let pitch buf random =
  let greens = [|0x528958; 0x5c9360; 0x568c59; 0x60945f|] in
  plane buf 0x667b49 (-2.) (-2.3) 102. 65.5 0;
  for stripe = 0 to 9 do
    let x = float (stripe * 10) in
    plane buf greens.(stripe mod 4) x 0. (x +. 10.) 64. 0
  done;
  let pale = [|0x679a5e; 0x719f60; 0x74a46a; 0x62955b|] in
  let deep = [|0x487f52; 0x508657; 0x548a52; 0x508656|] in
  for i = 0 to 12000 do
    let x = random_float random 99.8 in
    let y = random_float random 63.8 in
    let px, py = at x y 0 in
    let color = if i mod 3 = 0 then pale.(int_of_float x / 10 mod 4) else deep.(int_of_float x / 10 mod 4) in
    pixel buf px py color
  done;
  let chalk = 0xe1e7bb in
  let mark x0 y0 x1 y1 =
    let px, py = at x0 y0 0 and qx, qy = at x1 y1 0 in
    line buf (px, py + 1) (qx, qy + 1) 0x79a374;
    line buf (px, py) (qx, qy) chalk
  in
  let outline x0 y0 x1 y1 =
    mark x0 y0 x1 y0;
    mark x1 y0 x1 y1;
    mark x1 y1 x0 y1;
    mark x0 y1 x0 y0
  in
  outline 0. 0. 100. 64.;
  mark 50. 0. 50. 64.;
  outline 0. 14. 15. 50.;
  outline 85. 14. 100. 50.;
  outline 0. 23. 5. 41.;
  outline 95. 23. 100. 41.;
  (* Turn a point by a fixed angle for each segment of the chalk line. *)
  let arc x y radius steps (cos_step, sin_step) (cos_start, sin_start) =
    let point cos_a sin_a = at (x +. cos_a *. radius) (y +. sin_a *. radius) 0 in
    let rec draw remaining cos_a sin_a =
      if remaining > 0 then (
        let cos_next = cos_a *. cos_step -. sin_a *. sin_step in
        let sin_next = sin_a *. cos_step +. cos_a *. sin_step in
        line buf (point cos_a sin_a) (point cos_next sin_next) chalk;
        draw (remaining - 1) cos_next sin_next)
    in
    draw steps cos_start sin_start
  in
  let penalty_step = (0.992808636, 0.119712207) in
  let corner_step = (0.965925826, 0.258819045) in
  arc 50. 32. 9. 64 (0.995184727, 0.098017140) (1., 0.);
  arc 10.5 32. 8. 16 penalty_step (0.573519986, -0.819191568);
  arc 89.5 32. 8. 16 penalty_step (-0.573519986, 0.819191568);
  arc 0. 0. 2. 6 corner_step (1., 0.);
  arc 100. 0. 2. 6 corner_step (0., 1.);
  arc 100. 64. 2. 6 corner_step (-1., 0.);
  arc 0. 64. 2. 6 corner_step (0., -1.);
  List.iter (fun x ->
    let px, py = at x 32. 0 in
    rect buf (px - 1) py 3 2 chalk) [10.5; 50.; 89.5]

let tree buf x y size id =
  let ground_x, ground_y = at x y 0 in
  let head_y = ground_y - size in
  let radius = size * 42 / 100 in
  let shadow_x, shadow_y = at (min 104.5 (x +. float size *. 0.28)) (min 68.1 (y +. 1.3)) 0 in
  polygon buf 0x988c69
    [(ground_x - 2, ground_y); (shadow_x - 6, shadow_y - 6);
     (shadow_x + 1, shadow_y - 4); (shadow_x, shadow_y + 4);
     (shadow_x - 9, shadow_y + 4); (ground_x, ground_y + 2)];
  line buf (ground_x - 1, ground_y) (ground_x + 2, head_y + 3) 0x524936;
  line buf (ground_x, ground_y) (ground_x + 3, head_y + 3) 0x715c3d;
  line buf (ground_x + 1, ground_y) (ground_x + 4, head_y + 3) 0xa48252;
  line buf (ground_x + 1, ground_y - 9) (ground_x - 5, head_y + 6) 0x634f37;
  let lobe px py rx ry =
    for row = -ry to ry do
      let ratio = float row /. float ry in
      let dx = Float.to_int (float rx *. sqrt (1. -. ratio *. ratio)) in
      for col = -dx to dx do
        let detail = hash (px + col) (py + row + id) mod 24 in
        let edge = abs col >= dx - 1 in
        let bottom = row > ry / 4 || col > rx / 2 in
        let color =
          if edge || row = ry then (if detail > 16 then 0x38543d else 0x2e4939)
          else if bottom then (if detail > 16 then 0x587047 else 0x405b41)
          else if detail < 5 then 0x8e965a
          else if detail < 12 then 0x748653
          else 0x607950
        in
        pixel buf (px + col) (py + row) color
      done
    done
  in
  lobe (ground_x + radius / 2) (head_y + radius / 2) (max 4 (radius * 3 / 4)) (max 3 (radius * 2 / 3));
  lobe (ground_x - radius / 2) (head_y + radius / 4) (max 4 (radius * 3 / 4)) (max 3 (radius / 2));
  lobe (ground_x + radius / 4) (head_y - radius / 3) (max 3 (radius * 3 / 4)) (max 3 (radius * 3 / 5));
  lobe (ground_x - radius / 5) (head_y + radius / 4) (max 4 (radius * 2 / 3)) (max 3 (radius / 2))

let fence buf ~x0 ~y0 ~x1 ~y1 ~posts =
  let position i =
    let t = float i /. float posts in
    (x0 +. (x1 -. x0) *. t, y0 +. (y1 -. y0) *. t)
  in
  line buf (at x0 y0 2) (at x1 y1 2) 0x776c54;
  line buf (at x0 y0 7) (at x1 y1 7) 0x877e61;
  line buf (at x0 y0 8) (at x1 y1 8) 0xb4ac82;
  for i = 0 to posts do
    let x, y = position i in
    line buf (at x y 0) (at x y 8) 0x4d5748;
    let px, py = at x y 8 in
    pixel buf px py 0xd8c49a;
    if i < posts then (
      let ax, ay = position (i + 1) in
      line buf (at x y 2) (at ax ay 7) 0x948669)
  done

let goal buf x dir =
  let back = x +. 4.0 *. dir in
  let top = 13 in
  let start = 24. and stop = 40. in
  polygon buf 0x5b7549
    [at x start 0; at back start 0; at back stop 0; at x stop 0];
  let net = 0xa7b58b and light = 0xc3cca5 in
  for i = 0 to 8 do
    let y = start +. float (i * 2) in
    line buf (at back y 0) (at back y (top - 2)) net;
    line buf (at back y (top - 2)) (at x y top) light
  done;
  for z = 2 to 5 do
    let h = (z - 1) * 2 in
    line buf (at back start h) (at back stop h) net;
    line buf (at back start h) (at x start (h + 2)) net;
    line buf (at back stop h) (at x stop (h + 2)) net
  done;
  line buf (at back start 0) (at back stop 0) 0x8b9c71;
  line buf (at back start (top - 2)) (at back stop (top - 2)) 0xe3e2bd;
  List.iter (fun y ->
    line buf (at back y 0) (at back y (top - 2)) 0xd4d7af;
    line buf (at back y (top - 2)) (at x y top) 0xe5e2c1;
    line buf (at x y top) (at x y 0) white;
    let px, py = at x y top in
    line buf (px + 1, py + 1) (px + 1, py + top - 1) 0x989e80) [start; stop];
  line buf (at x start (top - 1)) (at x stop (top - 1)) 0xb6bc9a;
  line buf (at x start top) (at x stop top) white

let flag buf x y dir =
  let px, py = at x y 0 in
  line buf (px - 1, py) (px + 6, py + 2) 0x405b3e;
  line buf (px, py) (px, py - 11) 0x686548;
  line buf (px - 1, py - 1) (px - 1, py - 11) 0xf0e7bd;
  polygon buf 0xde6a4d [(px, py - 12); (px + dir * 7, py - 9); (px, py - 7)];
  line buf (px, py - 12) (px + dir * 6, py - 10) 0xf59e67

let stand buf =
  (* Two open blocks on the far side, with aisle markings under the roof. *)
  let x0 = 24. and x1 = 87. in
  plane buf 0xa29271 22. (-11.7) 89. (-1.8) 0;
  let palette = [|0x7b4643; 0xd98d64; 0xca6049; 0xe0cbab; 0x526a9d; 0x91a4af; 0x53694a; 0x9d8661|] in
  let complexions = [|0xe3b78a; 0xbd8a63; 0x9d6d52; 0xe6c098|] in
  for row = 0 to 3 do
    let y = -11.5 +. float row *. 2.25 in
    let z = 9 - row * 2 in
    plane buf (if row mod 2 = 0 then 0x85907a else 0x73846f) x0 y x1 (y +. 2.3) z;
    edge buf 0x485d50 x0 (y +. 2.3) x1 (y +. 2.3) z (z - 2);
    List.iter (fun x ->
      plane buf 0xbcad8a x y (x +. 2.4) (y +. 2.3) z;
      line buf (at x (y +. 2.1) z) (at (x +. 2.4) (y +. 2.1) z) 0xe3d1a5)
      [24.; 44.; 65.; 84.6];
    for i = 0 to 41 do
      let jitter = hash i (row + 911) in
      let x = 26.5 +. float i *. 1.40 +. float (jitter mod 5) *. 0.06 in
      let in_aisle = (x > 43.7 && x < 46.8) || (x > 64.4 && x < 67.7) in
      if not in_aisle && jitter mod 10 < 8 then (
        let px, py = at x (y +. 1.2) z in
        let shirt = palette.(jitter mod Array.length palette) in
        let skin = complexions.((jitter / 16) mod 4) in
        rect buf (px - 1) (py - 2) 4 3 0x3c5146;
        rect buf (px - 1) (py - 4) 3 3 shirt;
        rect buf px (py - 6) 2 2 skin;
        pixel buf (px + 1) (py - 7) 0x424136)
    done
  done;
  List.iter (fun x ->
    line buf (at x (-11.5) 10) (at x (-11.5) 32) 0x31483f;
    line buf (at x (-2.) 2) (at x (-2.) 22) 0x344d44;
    let px, py = at x (-2.) 22 in
    line buf (px + 1, py) (px + 1, py + 20) 0x799183)
    [24.; 45.; 66.; 87.];
  let roof x0 x1 =
    polygon buf 0x324e47
      [at x0 (-12.3) 31; at x1 (-12.3) 31; at x1 (-6.1) 25; at x0 (-6.1) 25];
    polygon buf 0x719188
      [at x0 (-12.3) 34; at x1 (-12.3) 34; at x1 (-6.1) 27; at x0 (-6.1) 27];
    for i = 0 to 34 do
      let x = x0 +. (x1 -. x0) *. float i /. 34. in
      line buf (at x (-12.2) 34) (at x (-6.2) 27)
        (if i mod 3 = 0 then 0xa0ad90 else 0x5d7b73)
    done;
    line buf (at x0 (-12.3) 34) (at x1 (-12.3) 34) 0xb7be9d;
    line buf (at x0 (-6.1) 27) (at x1 (-6.1) 27) 0xbdc5a0;
    line buf (at x0 (-6.1) 25) (at x1 (-6.1) 25) 0x344b43
  in
  roof 22.5 54.2;
  roof 55. 88.5;
  (* Painted boards follow the terrace instead of covering the touchline. *)
  for i = 0 to 5 do
    let x = 25. +. float i *. 10. in
    let color = [|0x536a63; 0xbd6f56; 0x778383; 0x596f59; 0x9e6b54; 0x627b78|].(i) in
    edge buf color x (-1.55) (x +. 9.5) (-1.55) 2 7;
    line buf (at x (-1.55) 7) (at (x +. 9.5) (-1.55) 7) 0xe1cf9b;
    for k = 0 to 6 do
      let xx = x +. 0.9 +. float k *. 1.1 in
      let px, py = at xx (-1.55) (4 + (k land 1)) in
      rect buf px py 2 (1 + k mod 3) 0xe2c89c
    done
  done

let clubhouse buf =
  let x0, x1 = (2.5, 14.5) and y0, y1 = (-11.2, -4.) in
  polygon buf 0xbba582 [at x0 y1 0; at x1 y1 0; at (x1 +. 4.) (-2.) 0; at (x0 +. 2.) (-2.) 0];
  edge buf 0xbe9872 x0 y1 x1 y1 0 14;
  edge buf 0xa27f61 x1 y0 x1 y1 0 14;
  for row = 0 to 4 do
    let z = 2 + row * 3 in
    line buf (at x0 y1 z) (at x1 y1 z) 0xd4ad82;
    for i = 0 to 7 do
      let x = x0 +. float i *. 1.53 +. float (row mod 2) *. 0.55 in
      line buf (at x y1 (z - 2)) (at x y1 (z - 1)) 0xac8768
    done
  done;
  edge buf 0x5a695d 3.8 y1 6.6 y1 4 10;
  edge buf 0x8b9d8b 4.2 y1 6.2 y1 7 9;
  edge buf 0x5d6d61 7.8 y1 10.7 y1 4 10;
  edge buf 0x859786 8.1 y1 10.4 y1 7 9;
  edge buf 0x45574c 11.9 y1 14. y1 0 10;
  edge buf 0x768473 12.8 y1 13.5 y1 2 7;
  let low = -3.2 and high = -12. in
  let eave0 = x0 -. 0.7 and eave1 = x1 +. 0.7 in
  polygon buf 0x7a5046 [at eave1 low 13; at eave1 high 18; at eave1 high 21; at eave1 low 16];
  polygon buf 0xbb6c53 [at eave0 low 16; at eave1 low 16; at eave1 high 21; at eave0 high 21];
  for n = 0 to 12 do
    let x = eave0 +. float n *. (eave1 -. eave0) /. 12. in
    line buf (at x low 16) (at x high 21) (if n mod 3 = 0 then 0xd99067 else 0xa95e4d)
  done;
  line buf (at eave0 high 21) (at eave1 high 21) 0xe5aa73;
  line buf (at eave0 low 16) (at eave1 low 16) 0xf0af7d;
  line buf (at eave0 low 15) (at eave1 low 15) 0x795046

let floodlight buf x y tall =
  let px, py = at x y 0 in
  let top = py - tall in
  line buf (px, py) (px + 13, py + 4) 0x978a69;
  rect buf (px - 3) (py - 2) 6 4 0x746f57;
  line buf (px - 1, py - 2) (px - 1, top) 0x55645a;
  line buf (px, py - 2) (px, top) 0x95a08a;
  line buf (px + 1, py - 2) (px + 1, top) 0x404f45;
  line buf (px, top + 6) (px + 6, top - 1) 0x596b5c;
  line buf (px, top + 6) (px - 7, top - 1) 0x596b5c;
  polygon buf 0x354c43 [(px - 11, top - 5); (px + 11, top - 3); (px + 11, top + 4); (px - 11, top + 2)];
  for i = 0 to 4 do
    let lx = px - 9 + i * 4 and ly = top - 4 + i / 2 in
    rect buf lx ly 3 3 0xffedbf;
    rect buf lx (ly + 3) 3 1 0xa39878
  done;
  line buf (px - 11, top - 6) (px + 11, top - 4) 0x8e9e88

let foreground buf =
  (* Low, empty dugouts leave the near touchline unobstructed. *)
  let dugout x0 x1 =
    let y = 66.2 in
    edge buf 0x616d5c x0 y x1 y 0 7;
    edge buf 0x829284 x0 (y +. 1.2) x1 (y +. 1.2) 2 4;
    edge buf 0x344e46 x0 (y +. 1.2) x1 (y +. 1.2) 0 2;
    plane buf 0x7a9388 x0 y x1 (y +. 1.) 7;
    line buf (at x0 y 7) (at x1 y 7) 0xb7c2a4;
    line buf (at x0 (y +. 1.) 7) (at x1 (y +. 1.) 7) 0x536e63;
    for i = 0 to 6 do
      let x = x0 +. float i *. (x1 -. x0) /. 6. in
      line buf (at x y 0) (at x y 7) (if i mod 6 = 0 then 0x3d554b else 0x78897b)
    done
  in
  dugout 28. 43.;
  dugout 57. 72.;
  for i = 0 to 8 do
    let x = 46. +. float i in
    line buf (at x 65.8 0) (at x 67.7 0) (if i mod 2 = 0 then 0xcaaa8c else 0xe6cdb0)
  done;
  (* Cast entry markings into the front lip, where the walkway meets the steps. *)
  Pixel_font.draw (fun col row ->
    let x = 5. +. float col *. 0.38 in
    let px, py = at x 69. (-1 - row) in
    pixel buf px py 0xebcfab) ~x:0 ~y:0 "NORTH QUAY"

let stadium = lazy (
  let buf = Bytes.make length '\000' in
  let random = ref 7301 in
  earth buf random;
  pitch buf random;
  List.iter (fun (x, y, size, id) -> tree buf x y size id)
    [(-1.8, -8.2, 32, 13); (18.8, -8.1, 28, 29);
     (94.7, -9.6, 34, 61); (98.3, -6.3, 23, 107);
     (-3.9, 55., 13, 76); (-4.0, 62., 12, 88);
     (103.1, 11.4, 12, 41); (103.5, 15.2, 10, 66)];
  clubhouse buf;
  stand buf;
  floodlight buf 17. (-3.) 56;
  floodlight buf 95. (-3.4) 60;
  fence buf ~x0:(-3.6) ~y0:3.1 ~x1:(-3.6) ~y1:22. ~posts:5;
  fence buf ~x0:(-3.6) ~y0:43.2 ~x1:(-3.6) ~y1:51. ~posts:2;
  fence buf ~x0:103.3 ~y0:17.5 ~x1:103.3 ~y1:22. ~posts:1;
  fence buf ~x0:103.3 ~y0:41.8 ~x1:103.3 ~y1:63.6 ~posts:6;
  goal buf 0. (-1.);
  goal buf 100. 1.;
  flag buf 0. 0. 1;
  flag buf 100. 0. (-1);
  flag buf 100. 64. (-1);
  flag buf 0. 64. 1;
  foreground buf;
  buf)

let frame = lazy (Bytes.create length)

type player = {
  x : float;
  y : float;
  home : bool;
  keeper : bool;
  selected : bool;
  moving : bool;
  facing_x : float;
  facing_y : float;
  number : int;
}

let shadow buf x y player =
  let shadow_color = 0x3c6749 in
  ellipse buf (x + 3) (y + 1) 6 2 shadow_color;
  polygon buf shadow_color [(x - 2, y); (x + 3, y - 2); (x + 12, y + 1); (x + 10, y + 3)];
  if player.selected then (
    let color = if player.home then 0xf9df8a else 0xbcecf5 in
    ellipse buf x (y + 1) 7 3 color;
    ellipse buf x (y + 1) 5 2 0x477150)

let skin_tones = [|
  (0xefd2a0, 0xbe8a5e, 0x41352f);
  (0xb98259, 0x7d5b43, 0x2e2b27);
  (0xd8a575, 0x99654b, 0x594334);
  (0xf0bc8a, 0xb6815f, 0x3b312e);
  (0xa57250, 0x694b3e, 0x302c29);
  (0xebbc8c, 0xaf7558, 0x74513c);
  (0xd69a69, 0xa16e4f, 0x362d29);
|]

let sprite buf x y p ticks =
  let number = p.number mod Array.length skin_tones in
  let skin, shade, hair = skin_tones.(number) in
  let tilt = if 3.15 *. p.facing_x -. 1.77 *. p.facing_y >= 0. then 1 else -1 in
  let back = 1.20 *. p.facing_x +. 1.22 *. p.facing_y < -0.4 in
  let light, jersey, dark, shorts, socks =
    if p.keeper && p.home then (0xf7d274, 0xddab50, 0x85673d, 0x455147, 0xf2d17e)
    else if p.keeper then (0x9ce4d8, 0x63b6a8, 0x3b7569, 0x354c49, 0xa9e0d2)
    else if p.home then (0xff987a, 0xe56b52, 0x9b433d, 0xf4eacb, 0xe88060)
    else (0xa0bcf1, 0x6489d8, 0x3d5386, 0x324768, 0x81a1e3)
  in
  let stride = if p.moving then ((ticks / 2 + number * 3) mod 6) - 2 else 0 in
  let bob = if p.moving && stride = 0 then 1 else 0 in
  let body = y - bob in
  let limb foot knee skew near =
    line buf (x + knee, body - 5) (x + foot, y - 2 - skew) (if near then shade else 0x794f3d);
    line buf (x + knee, body - 4) (x + foot, y - 1 - skew) (if near then socks else dark);
    span buf (y - skew) (x + foot - 1) (x + foot + 1) ink
  in
  limb (tilt * (stride + 2)) tilt (if p.moving && stride < 0 then 1 else 0) false;
  limb (-tilt * (stride + 1)) (-tilt) (if p.moving && stride > 1 then 2 else 0) true;
  rect buf (x - 3) (body - 7) 6 3 ink;
  rect buf (x - 2) (body - 7) 4 3 shorts;
  pixel buf (x - 2) (body - 5) (if p.home && not p.keeper then 0xc8c4a9 else 0x51617a);
  let arm = if p.moving then if stride < 0 then -1 else 1 else 0 in
  line buf (x + tilt * 3, body - 10) (x + tilt * (4 + arm), body - 7) shade;
  rect buf (x - 3) (body - 11) 7 4 dark;
  rect buf (x - 2) (body - 11) 5 4 jersey;
  rect buf (x - 2) (body - 11) 1 3 light;
  if back then pixel buf x (body - 9) light
  else pixel buf (x + tilt) (body - 10) white;
  line buf (x - tilt * 3, body - 10) (x - tilt * (4 - arm), body - 7) skin;
  rect buf (x - 2) (body - 16) 5 5 ink;
  rect buf (x - 1) (body - 16) 4 2 hair;
  rect buf (x - 1) (body - 14) 4 3 (if back then hair else shade);
  if not back then (
    rect buf (x - 1 + tilt) (body - 14) 2 2 skin;
    pixel buf (x + tilt) (body - 14) 0x674737);
  if p.selected then (
    let color = if p.home then 0xffe493 else 0xc6edf7 in
    span buf (y - 23) (x - 4) (x + 4) ink;
    span buf (y - 22) (x - 3) (x + 3) color;
    span buf (y - 21) (x - 2) (x + 2) color;
    span buf (y - 20) (x - 1) (x + 1) color;
    pixel buf x (y - 19) ink)

let ball_sprite buf x y =
  span buf (y - 3) (x - 1) (x + 1) 0x283b31;
  rect buf (x - 2) (y - 2) 5 5 0x283b31;
  rect buf (x - 1) (y - 2) 3 4 white;
  pixel buf (x + 2) y 0xd0d3b0;
  pixel buf x (y + 1) 0x514f43;
  pixel buf (x - 1) (y - 2) 0xffffff

let draw ~players ~ball_x ~ball_y ~ball_z ~ticks =
  let buf = Lazy.force frame in
  Bytes.blit (Lazy.force stadium) 0 buf 0 length;
  let bx, by = project ~x:ball_x ~y:ball_y in
  let ball_height = int_of_float (Float.max 0. ball_z *. 1.6) in
  let objects = Array.map (fun p ->
    let x, y = project ~x:p.x ~y:p.y in
    shadow buf x y p;
    (y, x, p)) players in
  ellipse buf (bx + ball_height / 3) (by + 1) 3 1 0x365a40;
  Array.sort (fun (y, _, _) (y', _, _) -> Int.compare y y') objects;
  Array.iter (fun (y, x, p) -> sprite buf x y p ticks) objects;
  (* Keep the ball readable when several players converge on its position. *)
  ball_sprite buf bx (by - 3 - ball_height);
  buf

let scorebug_width = 168
let scorebug_height = 46

let board_rect ?(alpha = '\255') buf x y w h color =
  let r = Char.chr (color lsr 16) in
  let g = Char.chr (color lsr 8 land 255) in
  let b = Char.chr (color land 255) in
  for row = max 0 y to min (scorebug_height - 1) (y + h - 1) do
    for col = max 0 x to min (scorebug_width - 1) (x + w - 1) do
      rgba_write buf ((row * scorebug_width + col) * 4) r g b alpha
    done
  done

let board_text buf ~x ~y ~scale color label =
  Pixel_font.draw (fun col row ->
    board_rect buf (x + col * scale) (y + row * scale) scale scale color
  ) ~x:0 ~y:0 label

let centered_board_text buf ~center ~midline ~scale color label =
  let x = center - Pixel_font.measure label * scale / 2 in
  let y = midline - Pixel_font.height * scale / 2 in
  board_text buf ~x ~y ~scale color label

let scorebug_base = lazy (
  let buf = Bytes.make (scorebug_width * scorebug_height * 4) '\000' in
  board_rect ~alpha:'\029' buf 3 3 165 33 0x141b17;
  board_rect buf 1 0 164 34 0x1b2d28;
  board_rect buf 0 1 166 32 0x1b2d28;
  board_rect buf 2 0 162 1 0x65796a;
  board_rect buf 1 33 164 1 0x14251f;
  board_rect buf 5 6 23 22 0x82493e;
  board_rect buf 5 27 23 1 coral;
  centered_board_text buf ~center:16 ~midline:19 ~scale:1 white "POR";
  board_rect buf 84 6 23 22 0x3c5577;
  board_rect buf 84 27 23 1 lapis;
  centered_board_text buf ~center:95 ~midline:19 ~scale:1 white "ROV";
  board_rect buf 109 6 54 22 0x30493b;
  board_rect buf 109 27 54 1 0x58715d;
  centered_board_text buf ~center:56 ~midline:19 ~scale:1 0x9eae91 "-";
  let caption = "NORTH QUAY | 7-A-SIDE" in
  board_rect buf 6 39 2 2 coral;
  board_text buf ~x:11 ~y:(40 - Pixel_font.height / 2) ~scale:1 0x4d6053 caption;
  buf)

let last_scorebug = ref None

let draw_scorebug ~home_score ~away_score ~seconds =
  let key = (home_score, away_score, seconds) in
  match !last_scorebug with
  | Some (previous, buf) when previous = key -> buf
  | _ ->
      let buf = Bytes.copy (Lazy.force scorebug_base) in
      let score center n =
        let label = string_of_int n in
        let scale = if Pixel_font.measure label <= 11 then 2 else 1 in
        centered_board_text buf ~center ~midline:19 ~scale white label
      in
      score 42 home_score;
      score 70 away_score;
      let two_digits n = (if n < 10 then "0" else "") ^ string_of_int n in
      let clock = two_digits (seconds / 60) ^ ":" ^ two_digits (seconds mod 60) in
      let digit_slots = [|0; 6; 12; 14; 20|] in
      (* Keep the digits in fixed cells even when the supplied 1 is narrow. *)
      for i = 0 to 4 do
        let column = String.make 1 clock.[i] in
        let cell_width = if i = 2 then 1 else 5 in
        let x = 111 + 2 * digit_slots.(i) + cell_width - Pixel_font.measure column in
        board_text buf ~x ~y:(19 - Pixel_font.height) ~scale:2 0xffedbb column
      done;
      last_scorebug := Some (key, buf);
      buf
