type team = Home | Away
type phase = Kickoff of team | Playing | Goal of team | Full_time

type input = { dx : float; dy : float; pass : bool; shoot : bool }

type player = {
    slot : int;
    team : team;
    mutable x : float;
    mutable y : float;
    mutable facing_x : float;
    mutable facing_y : float;
}

type ball = {
    mutable x : float;
    mutable y : float;
    mutable vx : float;
    mutable vy : float;
    mutable owner : int option;
    mutable receiver : int option;
    mutable last_touch : int option;
    mutable lock : int;
}

type t = {
    players : player array;
    ball : ball;
    mutable home_score : int;
    mutable away_score : int;
    mutable phase : phase;
    mutable ticks_left : int;
    mutable tick : int;
    mutable pause : int;
}

let ticks_per_second = 30
let match_seconds = 180
let players_per_team = 7
let player_count = 2 * players_per_team
let width = 100.
let height = 64.
let goal_top = 24.
let goal_bottom = 40.
let kickoff_pause = 27
let goal_pause = 45
let dribble_offset = 1.12
let first_touch = 20
let bot_first_touch = 12
let challenge_radius = 4.0
let human_run_speed = 0.81
let human_carry_speed = 0.73
let idle = { dx = 0.; dy = 0.; pass = false; shoot = false }

let clamp lower upper value = max lower (min upper value)
let square x = x *. x
let opposite = function Home -> Away | Away -> Home
let first_slot = function Home -> 0 | Away -> players_per_team
let team_of_slot slot = if slot < players_per_team then Home else Away
let direction = function Home -> 1. | Away -> -1.
let team_x team x = match team with Home -> x | Away -> width -. x
let role slot = slot mod players_per_team
let keeper slot = role slot = 0
let formation =
    [| (5., 32.); (22., 19.); (22., 45.); (38., 12.);
       (33., 32.); (38., 52.); (47., 32.) |]

let attach (ball : ball) (player : player) =
    ball.x <- clamp 0. width (player.x +. dribble_offset *. player.facing_x);
    ball.y <- clamp 0. height (player.y +. dribble_offset *. player.facing_y)

let take_ball ball player lock =
    ball.owner <- Some player.slot;
    ball.last_touch <- ball.owner;
    ball.receiver <- None;
    ball.vx <- 0.;
    ball.vy <- 0.;
    ball.lock <- lock;
    attach ball player

let kickoff game team =
    let width_shift = [| 0.; -2.5; 2.5; -1.2; 1.6 |].((game.tick / 47) mod 5) in
    Array.iter (fun player ->
        let x, y = formation.(role player.slot) in
        let kicking = player.team = team && role player.slot = 6 in
        player.x <- team_x player.team (if kicking then width *. 0.5 -. dribble_offset else x);
        player.y <- (match role player.slot with
            | 0 -> y
            | 6 -> if kicking then y else y +. 5. +. width_shift
            | _ -> y +. width_shift *. direction player.team);
        player.facing_x <- direction player.team;
        player.facing_y <- 0.
    ) game.players;
    take_ball game.ball game.players.(first_slot team + 6) first_touch;
    game.phase <- Kickoff team;
    game.pause <- kickoff_pause

let create () =
    let players = Array.init player_count (fun slot ->
        { slot; team = team_of_slot slot; x = 0.; y = 0.; facing_x = 0.; facing_y = 0. }
    ) in
    let ball =
        { x = 0.; y = 0.; vx = 0.; vy = 0.; owner = None;
          receiver = None; last_touch = None; lock = 0 }
    in
    let game =
        { players; ball; home_score = 0; away_score = 0; tick = 0;
          ticks_left = match_seconds * ticks_per_second; phase = Kickoff Home; pause = 0 }
    in
    kickoff game Home;
    game

let distance_sq (player : player) x y = square (player.x -. x) +. square (player.y -. y)

let in_challenge_range game slot =
    let player = game.players.(slot) and ball = game.ball in
    match ball.owner with
    | Some holder ->
        let holder = game.players.(holder) in
        holder.team <> player.team &&
        (distance_sq player holder.x holder.y <= square challenge_radius ||
         distance_sq player ball.x ball.y <= square challenge_radius)
    | None -> distance_sq player ball.x ball.y <= square challenge_radius

let closest_outfield ?(available = fun _ -> true) game team x y =
    let start = first_slot team in
    let best = ref (-1) in
    let distance = ref infinity in
    for slot = start + 1 to start + players_per_team - 1 do
        let candidate = distance_sq game.players.(slot) x y in
        if available slot && candidate < !distance then begin
            best := slot;
            distance := candidate
        end
    done;
    !best

let move (player : player) dx dy speed =
    let length_sq = square dx +. square dy in
    if length_sq > 0.001 then begin
        let length = sqrt length_sq in
        let travel = min length speed in
        player.facing_x <- dx /. length;
        player.facing_y <- dy /. length;
        player.x <- clamp 1.3 (width -. 1.3) (player.x +. travel *. player.facing_x);
        player.y <- clamp 1.3 (height -. 1.3) (player.y +. travel *. player.facing_y)
    end

let move_human ?(ticks = 1.) player input carrying =
    let axis value = if Float.is_finite value then clamp (-1.) 1. value else 0. in
    let dx = axis input.dx in
    let dy = axis input.dy in
    let speed = (if carrying then human_carry_speed else human_run_speed) *. ticks in
    move player (dx *. speed) (dy *. speed) speed

let dribble game (player : player) =
    let attack = direction player.team in
    let advance = team_x player.team player.x in
    let lane = if advance > 67. then clamp 25. 39. player.y
               else clamp 11. 53. player.y in
    let defender = game.players.(closest_outfield game (opposite player.team)
                       (player.x +. attack *. 2.5) player.y) in
    let ahead = attack *. (defender.x -. player.x) in
    if ahead > -0.3 && ahead < 5.5 && abs_float (defender.y -. player.y) < 3.6 then begin
        let side = if player.y < 7. || (player.y >= defender.y && player.y < 57.) then 1. else -1. in
        (player.x +. attack *. 3., defender.y +. side *. 5., 0.58)
    end else
        (team_x player.team 96., lane, 0.58)

let move_ai game player carrier home_chaser away_chaser =
    let ball = game.ball in
    let attack = direction player.team in
    let ball_x = team_x player.team ball.x in
    let attacking = match carrier with
        | Some owner -> owner.team = player.team
        | None -> false
    in
    let chaser = if player.team = Home then home_chaser else away_chaser in
    let target_x, target_y, speed =
        if keeper player.slot then begin
            if ball.owner = Some player.slot then
                (team_x player.team 7., clamp 23. 41. player.y, 0.34)
            else if ball.owner = None && ball_x < 11.
                    && abs_float ball.vx +. abs_float ball.vy < 0.67
                    && ball.y > 18. && ball.y < 46. then
                (team_x player.team (clamp 1.3 8.5 ball_x), ball.y, 0.45)
            else
                let shot = ball.owner = None && ball.receiver = None && ball.vx *. attack < -1.65 in
                let x = if ball_x < 22. then 3.7 else 5. in
                let y = if shot then
                            let flight = clamp 0. 10. ((team_x player.team x -. ball.x) /. ball.vx) in
                            clamp 24. 40. (ball.y +. ball.vy *. flight)
                        else if ball_x < 24. then clamp 23. 41. ball.y
                        else 32. +. (ball.y -. 32.) *. 0.24 in
                let speed = if shot && ball.lock > 4 then 0.13 else if shot then 0.34 else 0.40 in
                (team_x player.team x, y, speed)
        end else if ball.owner = Some player.slot then
            dribble game player
        else if ball.receiver = Some player.slot || (not attacking && player.slot = chaser) then
            (ball.x +. 3. *. ball.vx, ball.y +. 3. *. ball.vy, 0.59)
        else
            let x, y = formation.(role player.slot) in
            let advance = (ball_x -. 50.) *. 0.46 +. (if attacking then 11. else -2.) in
            let near_ball = max 0. (1. -. abs_float (ball.y -. y) /. 23.) in
            let support = if attacking then near_ball *. 7. else 0. in
            let shift = if not attacking && role player.slot <= 2 && ball_x < 34.
                        then (ball.y -. y) *. 0.48
                        else (ball.y -. 32.) *. 0.19 in
            (clamp 9. 91. (team_x player.team (x +. advance) +. attack *. support),
             clamp 6. 58. (y +. shift), 0.47)
    in
    let dx = target_x -. player.x and dy = target_y -. player.y in
    if square dx +. square dy > 0.16 then move player dx dy speed

let separate_players (players : player array) =
    for first = 0 to player_count - 2 do
        let a = players.(first) in
        for second = first + 1 to player_count - 1 do
            let b = players.(second) in
            let dx = b.x -. a.x and dy = b.y -. a.y in
            let length_sq = square dx +. square dy in
            if length_sq < square 1.85 then begin
                let length = sqrt length_sq in
                let nx, ny = if length < 0.001 then (0., 1.) else (dx /. length, dy /. length) in
                let push = (1.85 -. length) *. 0.5 in
                a.x <- clamp 1.3 (width -. 1.3) (a.x -. nx *. push);
                a.y <- clamp 1.3 (height -. 1.3) (a.y -. ny *. push);
                b.x <- clamp 1.3 (width -. 1.3) (b.x +. nx *. push);
                b.y <- clamp 1.3 (height -. 1.3) (b.y +. ny *. push)
            end
        done
    done

let kick (ball : ball) player receiver x y speed =
    let dx = x -. ball.x and dy = y -. ball.y in
    let length = sqrt (square dx +. square dy) in
    if length > 0.001 then begin
        ball.vx <- speed *. (dx /. length);
        ball.vy <- speed *. (dy /. length);
        ball.owner <- None;
        ball.receiver <- receiver;
        ball.last_touch <- Some player.slot;
        ball.lock <- 7
    end

let lane_distance_sq (player : player) x y dx dy length_sq =
    let px = player.x -. x and py = player.y -. y in
    let along = clamp 0. 1. ((px *. dx +. py *. dy) /. length_sq) in
    square (px -. dx *. along) +. square (py -. dy *. along)

let pass game passer human =
    let start = first_slot passer.team in
    let opponents = first_slot (opposite passer.team) in
    let best = ref None in
    let best_score = ref (if human || keeper passer.slot then neg_infinity else 1.5) in
    for slot = start + 1 to start + players_per_team - 1 do
        if slot <> passer.slot then begin
            let teammate = game.players.(slot) in
            let dx = teammate.x -. game.ball.x and dy = teammate.y -. game.ball.y in
            let length_sq = square dx +. square dy in
            if length_sq > square 3. && length_sq < square 43. then begin
                let length = sqrt length_sq in
                let clear = ref (square 5.) in
                for opponent = opponents to opponents + players_per_team - 1 do
                    let defender = game.players.(opponent) in
                    clear := min !clear
                        (lane_distance_sq defender game.ball.x game.ball.y dx dy length_sq)
                done;
                let aim = (dx *. passer.facing_x +. dy *. passer.facing_y) /. length in
                let score = direction passer.team *. dx *. 0.30 +. sqrt !clear *. 3.4
                            -. abs_float (length -. 16.) *. 0.30
                            +. (if human then 16. else 1.2) *. aim in
                if score > !best_score then begin
                    best := Some teammate;
                    best_score := score
                end
            end
        end
    done;
    match !best with
    | Some teammate ->
        let x = clamp 1.4 (width -. 1.4)
            (teammate.x +. direction teammate.team *. 1.4 +. teammate.facing_x *. 0.8) in
        let y = clamp 1.4 (height -. 1.4) (teammate.y +. teammate.facing_y *. 0.8) in
        let range = sqrt (distance_sq teammate game.ball.x game.ball.y) in
        let speed = clamp 1.75 2.12 (1.55 +. range *. 0.023) in
        kick game.ball passer (Some teammate.slot) x y speed
    | None when human ->
        kick game.ball passer None
            (game.ball.x +. 8. *. passer.facing_x)
            (game.ball.y +. 8. *. passer.facing_y) 1.33
    | None -> ()

let shot_target ?aim game shooter =
    let goalkeeper = game.players.(first_slot (opposite shooter.team)) in
    match aim with
    | Some lateral when abs_float lateral > 0.27 -> 32. +. 7.1 *. lateral
    | _ ->
        let offset = if Option.is_some aim then 6.8 else
            match (game.tick / 11 + role shooter.slot) mod 4 with
            | 0 -> 3.8 | 1 -> 4.5 | 2 -> 5.4 | _ -> 4.2
        in
        if goalkeeper.y >= 32. then 32. -. offset else 32. +. offset

let shoot ?aim game shooter =
    let x = team_x shooter.team (width +. 2.) in
    let range = team_x shooter.team shooter.x in
    let speed = if Option.is_some aim then clamp 1.9 2.6 (1.9 +. (range -. 43.) *. 0.032)
                else clamp 1.7 2.35 (1.68 +. (range -. 45.) *. 0.018) in
    kick game.ball shooter None x (shot_target ?aim game shooter) speed

let clear_shot game player =
    let ball = game.ball in
    let target_x = team_x player.team (width +. 2.) in
    let dx = target_x -. ball.x in
    let dy = shot_target game player -. ball.y in
    let opponents = first_slot (opposite player.team) in
    let open_lane = ref true in
    for slot = opponents + 1 to opponents + players_per_team - 1 do
        let defender = game.players.(slot) in
        let ahead = (defender.x -. ball.x) *. direction player.team in
        if ahead >= 0. && ahead < 5.5
           && lane_distance_sq defender ball.x ball.y dx dy (square dx +. square dy) < square 1.3
        then open_lane := false
    done;
    !open_lane

let play_ball game inputs player =
    let ball = game.ball in
    attach ball player;
    match inputs.(player.slot) with
    | Some input ->
        if input.shoot then
            let aim = if Float.is_finite input.dy && abs_float input.dy > 0.001
                      then player.facing_y else 0. in
            shoot ~aim game player
        else if input.pass then pass game player true
    | None when ball.lock < 8 ->
        let attack_x = team_x player.team player.x in
        if not (keeper player.slot) && attack_x > 74. && abs_float (player.y -. 32.) < 20.
           && clear_shot game player then
            shoot game player
        else if keeper player.slot then pass game player false
        else if (game.tick + player.slot) mod 10 = 0 then begin
            let enemy = closest_outfield game (opposite player.team) ball.x ball.y in
            if distance_sq game.players.(enemy) ball.x ball.y < square 5.5
               || (attack_x > 82. && abs_float (player.y -. 32.) > 18.) then
                pass game player false
        end
    | None -> ()

let tackle game inputs holder chaser =
    let ball = game.ball in
    let unlocked = ball.lock = 0 in
    (* Give players a chance to tackle before a pressured bot can pass. *)
    if unlocked || (ball.lock <= first_touch - bot_first_touch && inputs.(holder.slot) = None) then begin
        let start = first_slot (opposite holder.team) in
        let best = ref None in
        let best_distance = ref infinity in
        for slot = start to start + players_per_team - 1 do
            let player = game.players.(slot) in
            let candidate = distance_sq player ball.x ball.y in
            match inputs.(slot) with
            | Some input when input.pass && in_challenge_range game slot && candidate < !best_distance ->
                best := Some player;
                best_distance := candidate
            | _ -> ()
        done;
        let winner = match !best with
            | Some _ as human -> human
            | None when unlocked ->
                let keeper_slot = start in
                let goalkeeper = game.players.(keeper_slot) in
                if inputs.(keeper_slot) = None && team_x goalkeeper.team ball.x < 12. &&
                   distance_sq goalkeeper ball.x ball.y < square 2.1 then Some goalkeeper
                else if chaser >= 0 && (game.tick + role chaser) mod 7 = 0 then (
                    let player = game.players.(chaser) in
                    if distance_sq player ball.x ball.y < square 2.15 then Some player else None
                ) else None
            | None -> None
        in
        Option.iter (fun player -> take_ball ball player first_touch) winner
    end

let goal game team y =
    (match team with
     | Home -> game.home_score <- game.home_score + 1
     | Away -> game.away_score <- game.away_score + 1);
    let ball = game.ball in
    ball.x <- team_x team width;
    ball.y <- y;
    ball.vx <- 0.;
    ball.vy <- 0.;
    ball.receiver <- None;
    ball.lock <- 0;
    game.phase <- Goal team;
    game.pause <- goal_pause

let restart game slot x y facing_x facing_y =
    let player = game.players.(slot) in
    player.x <- x;
    player.y <- y;
    player.facing_x <- facing_x;
    player.facing_y <- facing_y;
    take_ball game.ball player first_touch

let end_line game defending y =
    let corner = match game.ball.last_touch with
        | Some slot -> team_of_slot slot = defending
        | None -> false
    in
    if corner then begin
        let team = opposite defending in
        let x = team_x defending 2. in
        let y = if y < 32. then 2. else 62. in
        let slot = closest_outfield game team x y in
        restart game slot x y (direction defending *. 0.6) (if y < 32. then 0.8 else -0.8)
    end else
        restart game (first_slot defending) (team_x defending 5.) 32.
            (direction defending) 0.

let touch_line game x y =
    let team = match game.ball.last_touch with
        | Some slot -> opposite (team_of_slot slot)
        | None -> Home
    in
    let x = clamp 3. 97. x in
    let y = if y < 0. then 2. else 62. in
    let slot = closest_outfield game team x y in
    restart game slot x y 0. (if y < 32. then 1. else -1.)

let receive_ball game inputs x y dx dy =
    let ball = game.ball in
    let speed_sq = square ball.vx +. square ball.vy in
    let travel_sq = square dx +. square dy in
    let best = ref None in
    let distance = ref infinity in
    for i = 0 to player_count - 1 do
        let player = game.players.((i + game.tick) mod player_count) in
        if ball.lock = 0 || ball.last_touch <> Some player.slot then begin
            let human, challenge = match inputs.(player.slot) with
                | Some input -> true, input.pass
                | None -> false, false
            in
            let radius =
                if keeper player.slot && team_x player.team player.x < 12. then
                    (if ball.receiver = None && speed_sq >= square 1.65 then 1.58 else 2.25)
                else if ball.receiver = Some player.slot then
                    (if human then 3.3 else 2.45)
                else if speed_sq < square 0.85 then
                    (if challenge then challenge_radius else if human then 2.75 else 1.92)
                else if ball.receiver <> None then
                    (if challenge then 2.7 else if human then 1.70 else 1.24)
                else if challenge then 2.25 else 0.70
            in
            let candidate =
                if travel_sq > 0.00001 then lane_distance_sq player x y dx dy travel_sq
                else distance_sq player x y
            in
            if candidate < square radius && candidate < !distance then begin
                best := Some player;
                distance := candidate
            end
        end
    done;
    Option.iter (fun player -> take_ball ball player first_touch) !best

let move_ball game inputs =
    let ball = game.ball in
    let x = ball.x and y = ball.y in
    let next_x = x +. ball.vx and next_y = y +. ball.vy in
    let end_crossing = if next_x < 0. then (0. -. x) /. ball.vx
                       else if next_x > width then (width -. x) /. ball.vx else infinity in
    let side_crossing = if next_y < 0. then (0. -. y) /. ball.vy
                        else if next_y > height then (height -. y) /. ball.vy else infinity in
    let duration = min 1. (min end_crossing side_crossing) in
    let dx = ball.vx *. duration and dy = ball.vy *. duration in
    ball.x <- clamp 0. width (x +. dx);
    ball.y <- clamp 0. height (y +. dy);
    receive_ball game inputs x y dx dy;
    if ball.owner = None then begin
        if end_crossing < 1. && end_crossing <= side_crossing then begin
            let defending = if next_x < 0. then Home else Away in
            if ball.y > goal_top && ball.y < goal_bottom then goal game (opposite defending) ball.y
            else end_line game defending ball.y
        end else if side_crossing < 1. then touch_line game ball.x next_y
        else begin
            let drag = if ball.receiver <> None then 0.982 else 0.985 in
            ball.vx <- ball.vx *. drag;
            ball.vy <- ball.vy *. drag;
            if square ball.vx +. square ball.vy < square 0.06 then begin
                ball.vx <- 0.;
                ball.vy <- 0.;
                ball.receiver <- None
            end
        end
    end

let play game inputs =
    let ball = game.ball in
    let carrier = Option.map (Array.get game.players) ball.owner in
    let available slot = inputs.(slot) = None in
    let home_chaser = closest_outfield ~available game Home ball.x ball.y in
    let away_chaser = closest_outfield ~available game Away ball.x ball.y in
    Array.iter (fun player ->
        match inputs.(player.slot) with
        | Some input -> move_human player input (ball.owner = Some player.slot)
        | None -> move_ai game player carrier home_chaser away_chaser
    ) game.players;
    separate_players game.players;
    if ball.lock > 0 then ball.lock <- ball.lock - 1;
    match carrier with
    | Some player ->
        play_ball game inputs player;
        if ball.owner = None then move_ball game inputs
        else tackle game inputs player (if player.team = Home then away_chaser else home_chaser)
    | None -> move_ball game inputs

let step game inputs =
    (* The kickoff taker can start with a kick before the countdown expires. *)
    (match game.phase with
     | Kickoff team ->
         (match inputs.(first_slot team + 6) with
          | Some input when input.pass || input.shoot ->
              game.phase <- Playing;
              game.pause <- 0
          | _ -> ())
     | _ -> ());
    match game.phase with
    | Full_time -> ()
    | Kickoff _ ->
        game.tick <- game.tick + 1;
        game.pause <- game.pause - 1;
        if game.pause = 0 then game.phase <- Playing
    | Goal team ->
        game.tick <- game.tick + 1;
        game.pause <- game.pause - 1;
        if game.pause = 0 then kickoff game (opposite team)
    | Playing ->
        game.tick <- game.tick + 1;
        game.ticks_left <- game.ticks_left - 1;
        play game inputs;
        if game.ticks_left = 0 then begin
            game.phase <- Full_time;
            game.pause <- 0
        end

(* TLM1 wire snapshot. The header is magic, tick, clock, scores, phase, pause,
   owner, receiver, last touch, lock. Each slot then has x, y and its facing
   vector, followed by the ball position and velocity. All numbers are little
   endian, and coordinates are binary64 so resumption is exact. *)
let players_offset = 20
let ball_offset = players_offset + player_count * 32
let snapshot_size = ball_offset + 32

let encode game =
    let bytes = Bytes.create snapshot_size in
    let byte offset value = Bytes.set_uint8 bytes offset value in
    let uint offset value = Bytes.set_uint16_le bytes offset value in
    let float offset value = Bytes.set_int64_le bytes offset (Int64.bits_of_float value) in
    let slot offset = function None -> byte offset 255 | Some value -> byte offset value in
    Bytes.blit_string "TLM1" 0 bytes 0 4;
    Bytes.set_int32_le bytes 4 (Int32.of_int game.tick);
    uint 8 game.ticks_left;
    uint 10 game.home_score;
    uint 12 game.away_score;
    byte 14 (match game.phase with
        | Playing -> 0 | Kickoff Home -> 1 | Kickoff Away -> 2
        | Goal Home -> 3 | Goal Away -> 4 | Full_time -> 5);
    byte 15 game.pause;
    slot 16 game.ball.owner;
    slot 17 game.ball.receiver;
    slot 18 game.ball.last_touch;
    byte 19 game.ball.lock;
    Array.iter (fun player ->
        let offset = players_offset + 32 * player.slot in
        float offset player.x;
        float (offset + 8) player.y;
        float (offset + 16) player.facing_x;
        float (offset + 24) player.facing_y
    ) game.players;
    float ball_offset game.ball.x;
    float (ball_offset + 8) game.ball.y;
    float (ball_offset + 16) game.ball.vx;
    float (ball_offset + 24) game.ball.vy;
    Bytes.unsafe_to_string bytes

exception Invalid_snapshot

let decode bytes =
    if String.length bytes <> snapshot_size || String.sub bytes 0 4 <> "TLM1" then None
    else try
        let byte offset = String.get_uint8 bytes offset in
        let uint offset = String.get_uint16_le bytes offset in
        let float offset min max =
            let value = Int64.float_of_bits (String.get_int64_le bytes offset) in
            if not (value >= min && value <= max) then raise Invalid_snapshot;
            value
        in
        let slot offset =
            let value = byte offset in
            if value = 255 then None
            else if value < player_count then Some value
            else raise Invalid_snapshot
        in
        let phase = match byte 14 with
            | 0 -> Playing | 1 -> Kickoff Home | 2 -> Kickoff Away
            | 3 -> Goal Home | 4 -> Goal Away | 5 -> Full_time
            | _ -> raise Invalid_snapshot
        in
        let tick32 = String.get_int32_le bytes 4 in
        let ticks_left = uint 8 and pause = byte 15 in
        if tick32 < 0l || tick32 > 100_000l || ticks_left > match_seconds * ticks_per_second
           || (ticks_left = 0) <> (phase = Full_time) then raise Invalid_snapshot;
        (match phase with
         | Playing | Full_time when pause = 0 -> ()
         | Kickoff _ when pause > 0 && pause <= kickoff_pause -> ()
         | Goal _ when pause > 0 && pause <= goal_pause -> ()
         | _ -> raise Invalid_snapshot);
        let players = Array.init player_count (fun slot ->
            let offset = players_offset + 32 * slot in
            { slot; team = team_of_slot slot;
              x = float offset 1.3 (width -. 1.3);
              y = float (offset + 8) 1.3 (height -. 1.3);
              facing_x = float (offset + 16) (-1.) 1.;
              facing_y = float (offset + 24) (-1.) 1. }
        ) in
        let ball =
            { x = float ball_offset 0. width;
              y = float (ball_offset + 8) 0. height;
              vx = float (ball_offset + 16) (-2.6) 2.6;
              vy = float (ball_offset + 24) (-2.6) 2.6;
              owner = slot 16; receiver = slot 17; last_touch = slot 18; lock = byte 19 }
        in
        if ball.lock > first_touch
           || (ball.owner <> None &&
                (ball.owner <> ball.last_touch || ball.receiver <> None || ball.vx <> 0. || ball.vy <> 0.))
           || (ball.receiver <> None && ball.last_touch = None)
        then raise Invalid_snapshot;
        Some { players; ball; home_score = uint 10; away_score = uint 12; phase;
               ticks_left; tick = Int32.to_int tick32; pause }
    with Invalid_snapshot -> None
