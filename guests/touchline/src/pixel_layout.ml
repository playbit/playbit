(* Playbit lays out a canvas in logical units and samples its bitmaps with
   nearest-neighbor filtering. Pick a rectangle whose backing pixels divide
   evenly into source pixels. Extra texels at the right and bottom stay clear. *)
type box = { x : int; y : int; w : int; h : int }
type image = { w : int; h : int; rgba : bytes }
type fit = {
  box : box;
  texels_w : int;
  texels_h : int;
  zoom : int;     (* device pixels per displayed texel *)
  sample : int;   (* source pixels per texel in very small windows *)
}

let contains (b : box) x y =
  x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h

let device density length =
  let product = Int32.float_of_bits (Int32.bits_of_float (float length *. density)) in
  int_of_float (ceil product)

let div_up n by = (n + by - 1) / by

let fit ~density ~max_zoom (area : box) ~w ~h =
  let physical_w = device density area.w and physical_h = device density area.h in
  if min physical_w physical_h < 1 || min w h < 1 then None
  else
    let rec sampling factor =
      if div_up w factor <= physical_w && div_up h factor <= physical_h then factor
      else sampling (2 * factor)
    in
    let sample = sampling 1 in
    let sw = div_up w sample and sh = div_up h sample in
    let axis source limit zoom =
      (* Alignment can add a few clear texels. Bound that padding by the
         display scale so even a wall-sized window keeps a small source bitmap. *)
      let max_texels = source + int_of_float (ceil density) in
      let rec first_length target lo hi =
        if lo = hi then lo
        else
          let mid = lo + (hi - lo) / 2 in
          if device density mid < target then first_length target (mid + 1) hi
          else first_length target lo mid
      in
      let rec aligned texels =
        if texels > max_texels then None
        else
          let target = texels * zoom in
          let length = first_length target 1 limit in
          if device density length = target then Some (length, texels)
          else aligned (texels + 1)
      in
      aligned source
    in
    let rec choose zoom =
      match axis sw area.w zoom, axis sh area.h zoom with
      | Some (width, texels_w), Some (height, texels_h) ->
          let box = { x = area.x + (area.w - width) / 2; y = area.y + (area.h - height) / 2;
                      w = width; h = height } in
          Some { box; texels_w; texels_h; zoom; sample }
      | _ when zoom > 1 -> choose (zoom - 1)
      | _ -> None
    in
    choose (min max_zoom (min (physical_w / sw) (physical_h / sh)))

let pixels fit (image : image) =
  if fit.sample = 1 && image.w = fit.texels_w && image.h = fit.texels_h then image.rgba
  else
    let out = Bytes.make (4 * fit.texels_w * fit.texels_h) '\000' in
    let sample = fit.sample in
    for y = 0 to div_up image.h sample - 1 do
      let row = 4 * y * fit.texels_w in
      if sample = 1 then Bytes.blit image.rgba (4 * y * image.w) out row (4 * image.w)
      else (
        let sy = min (image.h - 1) (y * sample + sample / 2) in
        for x = 0 to div_up image.w sample - 1 do
          let sx = min (image.w - 1) (x * sample + sample / 2) in
          Bytes.blit image.rgba (4 * (sy * image.w + sx)) out (row + 4 * x) 4
        done)
    done;
    out

let to_source fit x y =
  let b = fit.box in
  (float (x - b.x) *. float (fit.sample * fit.texels_w) /. float b.w,
   float (y - b.y) *. float (fit.sample * fit.texels_h) /. float b.h)
