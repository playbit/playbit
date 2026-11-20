open Pixel_layout

let frame ~density ~w ~h =
  let area = { x = 3; y = 5; w; h } in
  let fit = Option.get (fit ~density ~max_zoom:max_int area ~w:506 ~h:294) in
  let b = fit.box in
  assert (b.x >= area.x && b.x + b.w <= area.x + w);
  assert (b.y >= area.y && b.y + b.h <= area.y + h);
  assert (device density b.w = fit.texels_w * fit.zoom);
  assert (device density b.h = fit.texels_h * fit.zoom);
  assert (fit.texels_w >= div_up 506 fit.sample && fit.texels_h >= div_up 294 fit.sample);
  fit

let () =
  (* Crossing the exact threshold grows the whole pitch by one backing pixel
     per texel. Moving the window to another display repeats the calculation. *)
  assert ((frame ~density:2. ~w:505 ~h:400).zoom = 1);
  assert ((frame ~density:2. ~w:506 ~h:400).zoom = 2);
  assert ((frame ~density:1. ~w:320 ~h:400).sample = 2);
  List.iter (fun density ->
    for width = 200 to 1100 do
      ignore (frame ~density ~w:width ~h:512)
    done
  ) [1.; 1.25; 1.5; 2.; 2.5; 2.75; 3.];
  let odd = {w = 3; h = 1; rgba = Bytes.of_string "aaaaBBBBcccc"} in
  let padded = Option.get (fit ~density:2. ~max_zoom:1 {x=0; y=0; w=2; h=1} ~w:3 ~h:1) in
  let result = pixels padded odd in
  assert (Bytes.sub_string result 0 12 = "aaaaBBBBcccc");
  assert (Bytes.sub_string result 12 20 = String.make 20 '\000');
  print_endline "Touchline pixel layout tests passed."
