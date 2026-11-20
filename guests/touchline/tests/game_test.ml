open Match

let check name condition = if not condition then failwith name

let no_bots () = Array.make player_count (Some idle)

let step_n game inputs count =
    for _ = 1 to count do
        step game inputs
    done

let playing inputs =
    let game = create () in
    step_n game inputs kickoff_pause;
    check "kickoff reaches play" (game.phase = Playing);
    game

let place (player : player) x y =
    player.x <- x;
    player.y <- y;
    player.facing_x <- direction player.team;
    player.facing_y <- 0.

let test_kickoff_and_movement () =
    let inputs = no_bots () in
    let game = create () in
    let forward = game.players.(6) in
    check "seven players on each side"
        (Array.length game.players = 14 &&
         Array.for_all (fun player -> player.team = team_of_slot player.slot) game.players);
    check "the kickoff places the ball at center"
        (game.phase = Kickoff Home && game.ball.owner = Some 6 && game.ball.x = 50. && game.ball.y = 32.);
    let start_x = forward.x and start_y = forward.y in
    inputs.(6) <- Some { idle with dx = 1.; dy = -1. };
    step_n game inputs kickoff_pause;
    check "kickoff countdown preserves positions and clock"
        (forward.x = start_x && forward.y = start_y && game.ticks_left = 5400);
    step game inputs;
    check "directional input controls the dribbler"
        (forward.x > start_x && forward.y < start_y && forward.facing_y < 0.);
    check "diagonal speed is normalized"
        (abs_float (distance_sq forward start_x start_y -. square human_carry_speed) < 1e-8);
    check "ball stays with the carrier" (game.ball.owner = Some 6 && game.ball.x > forward.x);
    check "play advances the clock" (game.ticks_left = 5399)

let test_pass_and_interception () =
    let inputs = no_bots () in
    let game = playing inputs in
    let passer = game.players.(6) and teammate = game.players.(4) in
    let prepare () =
        place passer 35. 32.;
        place teammate 51. 32.;
        take_ball game.ball passer 0;
        inputs.(6) <- Some { idle with pass = true };
        step game inputs;
        inputs.(6) <- Some idle
    in
    prepare ();
    check "pass releases toward a teammate" (game.ball.owner = None && game.ball.receiver = Some 4);
    step_n game inputs 20;
    check "teammate controls the pass" (game.ball.owner = Some 4);
    prepare ();
    let opponent = game.players.(13) in
    place opponent 42. 32.;
    step_n game inputs 10;
    check "an opponent can cut out the pass" (game.ball.owner = Some 13);
    check "interception clears the intended receiver" (game.ball.receiver = None)

let test_directional_pass () =
    List.iter (fun (dy, receiver) ->
        let inputs = no_bots () in
        let game = playing inputs in
        let passer = game.players.(6) in
        place passer 40. 32.;
        place game.players.(3) 54. 21.;
        place game.players.(5) 54. 43.;
        take_ball game.ball passer 0;
        inputs.(6) <- Some { idle with dx = 0.65; dy; pass = true };
        step game inputs;
        check "passing direction chooses between two open runners"
            (game.ball.receiver = Some receiver);
        inputs.(6) <- Some idle;
        step_n game inputs 18;
        check "the aimed pass arrives in front of the intended runner"
            (game.ball.owner = Some receiver && game.ball.lock > 0)
    ) [-1., 3; 1., 5]

let test_tackle () =
    List.iter (fun team ->
        let inputs = no_bots () in
        let game = playing inputs in
        let holder = first_slot team + 6 in
        let challenger = first_slot (opposite team) + 6 in
        place game.players.(holder) 50. 32.;
        place game.players.(challenger) 50. 35.5;
        take_ball game.ball game.players.(holder) 2;
        check "reach is independent of the first-touch shield"
            (in_challenge_range game challenger && not (in_challenge_range game holder));
        inputs.(challenger) <- Some { idle with pass = true };
        step game inputs;
        check "a tap cannot steal through the shield" (game.ball.owner = Some holder);
        inputs.(challenger) <- Some idle;
        step game inputs;
        check "human proximity alone does not tackle" (game.ball.owner = Some holder);
        inputs.(challenger) <- Some { idle with pass = true };
        step game inputs;
        check "either team can win the ball with a nearby tap"
            (game.ball.owner = Some challenger && game.ball.last_touch = Some challenger &&
             game.ball.lock = first_touch);
        inputs.(challenger) <- Some { idle with shoot = true };
        inputs.(holder) <- Some { idle with pass = true };
        step game inputs;
        check "a new carrier can shoot immediately"
            (game.ball.owner = None && game.ball.last_touch = Some challenger)
    ) [Home; Away]

let test_tackle_bot () =
    let inputs = no_bots () in
    let game = playing inputs in
    let holder = game.players.(13) and challenger = game.players.(6) in
    place holder 50. 32.;
    place challenger 50. 34.5;
    inputs.(13) <- None;
    inputs.(6) <- Some { idle with pass = true };
    take_ball game.ball holder (first_touch - bot_first_touch + 2);
    step game inputs;
    check "a bot still gets time to control a new ball" (game.ball.owner = Some 13);
    step game inputs;
    check "a player can tackle a bot before its pressure pass"
        (game.ball.owner = Some 6 && game.ball.lock = first_touch)

let test_loose_ball () =
    let inputs = no_bots () in
    let game = playing inputs in
    let player = game.players.(6) in
    place player 50. 34.5;
    game.ball.owner <- None;
    game.ball.receiver <- None;
    game.ball.last_touch <- None;
    game.ball.vx <- 0.5;
    game.ball.x <- 50.;
    game.ball.y <- 32.;
    step game inputs;
    check "a controlled player gathers an accessible loose ball"
        (game.ball.owner = Some 6 && game.ball.lock = first_touch)

let test_kickoff_run_and_shot () =
    (* On the tilted pitch, screen Right and Right+Down move along opposing flanks. *)
    List.iter (fun (dy, range) ->
        let game = create () in
        let inputs = Array.make player_count None in
        let striker = game.players.(6) in
        let windup = ref 0 in
        let scored = ref false in
        for _ = 1 to 220 do
            let input =
                if game.phase <> Playing || game.ball.owner <> Some 6 then idle
                else if striker.x < range then { idle with dx = 1.; dy }
                else begin
                    incr windup;
                    { idle with shoot = !windup = 7 }
                end
            in
            inputs.(6) <- Some input;
            step game inputs;
            if game.phase = Goal Home && game.ball.last_touch = Some 6 then scored := true
        done;
        check "solo player can carry from kickoff, stop and shoot across the keeper" !scored
    ) [-0.98, 64.; 0.65, 70.]

let test_idle_players_do_not_stop_pressure () =
    let game = create () in
    let inputs = Array.make player_count None in
    inputs.(6) <- Some idle;
    inputs.(13) <- Some idle;
    let turned_over = ref false in
    for _ = 1 to 120 do
        step game inputs;
        if game.ball.owner <> Some 6 then turned_over := true
    done;
    check "a nearby idle human does not prevent a bot from pressing" !turned_over

let test_shot_and_goal () =
    let inputs = no_bots () in
    let game = playing inputs in
    let striker = game.players.(6) and goalkeeper = game.players.(7) in
    place striker 87. 36.;
    place goalkeeper 96. 30.;
    take_ball game.ball striker 0;
    inputs.(6) <- Some { idle with shoot = true };
    step game inputs;
    inputs.(6) <- Some idle;
    check "shot releases the ball at pace" (game.ball.owner = None && game.ball.vx > 2.);
    step_n game inputs 6;
    check "a shot across the goal mouth scores once"
        (game.phase = Goal Home && game.home_score = 1 && game.away_score = 0);
    let time_left = game.ticks_left in
    step_n game inputs goal_pause;
    check "conceding team takes the following kickoff"
        (game.phase = Kickoff Away && game.ball.owner = Some 13 && game.ball.x = 50.);
    check "celebration and restart pause the clock" (game.ticks_left = time_left);
    inputs.(13) <- Some { idle with pass = true };
    step game inputs;
    check "kickoff taker can put the ball into play immediately"
        (game.phase = Playing && game.ball.owner <> Some 13 && game.ticks_left = time_left - 1)

let test_keeper_and_boundaries () =
    let inputs = no_bots () in
    let game = playing inputs in
    let send_ball x y vx vy last_touch =
        game.ball.x <- x;
        game.ball.y <- y;
        game.ball.vx <- vx;
        game.ball.vy <- vy;
        game.ball.owner <- None;
        game.ball.receiver <- None;
        game.ball.last_touch <- Some last_touch;
        game.ball.lock <- 0;
        step game inputs
    in
    place game.players.(7) 98.7 31.;
    send_ball 97.75 31. 2.6 0. 6;
    check "keeper can save at the line before the goal registers"
        (game.ball.owner = Some 7 && game.home_score = 0);
    send_ball 99.5 9. 2. 0. 6;
    check "attacking touch over the end line gives the keeper a goal kick"
        (game.ball.owner = Some 7 && game.ball.x > 90. && game.ball.y = 32.);
    send_ball 99.5 9. 2. 0. 7;
    check "defending touch over the end line gives the other side a corner"
        (Option.map team_of_slot game.ball.owner = Some Home && game.ball.y < 5.);
    send_ball 42. 0.6 0. (-2.) 6;
    check "touchline restart goes to the other side"
        (Option.map team_of_slot game.ball.owner = Some Away && game.ball.y < 5.)

let restore game =
    match decode (encode game) with
    | Some restored -> restored
    | None -> failwith "match snapshot should validate"

let test_snapshot () =
    let inputs = Array.make player_count None in
    let game = create () in
    let restored = restore game in
    check "kickoff snapshot round trips exactly" (encode restored = encode game);
    step_n game inputs 163;
    let restored = restore game in
    check "live snapshot round trips exactly" (encode restored = encode game);
    for _ = 1 to 900 do
        step restored inputs;
        step game inputs;
        check "restored game continues on the same simulation" (encode restored = encode game)
    done;
    let packet = encode game in
    let bad version_offset replacement =
        let buffer = Bytes.of_string packet in
        Bytes.set_uint8 buffer version_offset replacement;
        decode (Bytes.to_string buffer) = None
    in
    check "snapshot rejects truncated data"
        (decode (String.sub packet 0 (String.length packet - 1)) = None);
    check "snapshot rejects unknown versions" (bad 3 50);
    check "snapshot rejects out-of-range owner slots" (bad 16 player_count);
    let buffer = Bytes.of_string packet in
    Bytes.set_int64_le buffer ball_offset (Int64.bits_of_float nan);
    check "snapshot rejects nonfinite positions" (decode (Bytes.to_string buffer) = None)

let test_complete_bot_match () =
    let game = create () in
    let inputs = Array.make player_count None in
    let saw_pass = ref false and saw_shot = ref false in
    let play_ticks = ref 0 in
    for _ = 1 to 20_000 do
        if game.phase <> Full_time then begin
            if game.phase = Playing then incr play_ticks;
            step game inputs;
            if game.ball.receiver <> None then saw_pass := true;
            if game.ball.owner = None && abs_float game.ball.vx > 1.7 then saw_shot := true
        end
    done;
    check "bots pass and shoot" (!saw_pass && !saw_shot);
    check "both sides can score" (game.home_score > 0 && game.away_score > 0);
    check "match reaches full time on the playing clock"
        (game.phase = Full_time && game.ticks_left = 0 &&
         !play_ticks = match_seconds * ticks_per_second);
    let final = encode (restore game) in
    step game inputs;
    check "full time freezes the game" (encode game = final)

let () =
    test_kickoff_and_movement ();
    test_pass_and_interception ();
    test_directional_pass ();
    test_tackle ();
    test_tackle_bot ();
    test_loose_ball ();
    test_kickoff_run_and_shot ();
    test_idle_players_do_not_stop_pressure ();
    test_shot_and_goal ();
    test_keeper_and_boundaries ();
    test_snapshot ();
    test_complete_bot_match ();
    print_endline "Touchline match tests passed."
