let peer char = String.make 48 char
let new_client id = (id, Session.create ())
let client_session = snd
let clients = ref []
let blocked_state_for = ref None
let blocked_offers_for = ref None

let process (id, session) event =
    Playbit.current := id;
    Session.event session event

let flush () =
    while not (Queue.is_empty Playbit.outgoing) do
        let recipient, event = Queue.take Playbit.outgoing in
        List.iter (fun ((id, _) as client) ->
            if recipient = Some id || recipient = None then
                match event.Playbit.data with
                | Message (_, _, bytes) when !blocked_state_for = Some id
                  && (match Wire.decode bytes with Some (Wire.State _) -> true | _ -> false) -> ()
                | Message (_, _, bytes) when !blocked_offers_for = Some id
                  && (match Wire.decode bytes with Some (Wire.Offer _) -> true | _ -> false) -> ()
                | _ -> process client event
        ) !clients
    done

let membership participants =
    clients := participants;
    Playbit.members := List.map fst participants;
    List.iter (fun client -> process client { Playbit.kind = 5l; data = Empty }) participants;
    flush ()

let start (id, session) =
    Playbit.current := id;
    Session.start session

let frame controls =
    Playbit.now := Int64.add !Playbit.now 16666667L;
    List.iter (fun (id, session) ->
        Playbit.current := id;
        Session.frame session (1. /. 60.) (controls id)
    ) !clients;
    flush ()

let frames count =
    for _ = 1 to count do frame (fun _ -> Match.idle) done

let same_roster a b =
    match a.Session.live, b.Session.live with
    | Some a, Some b ->
        assert (a.round = b.round);
        assert (a.slots = b.slots)
    | _ -> failwith "Expected authoritative snapshots"

let occupied session = Option.get session.Session.live
let tick session = (Session.game session).Match.tick

let place (game : Match.t) slot x y =
    let player = game.players.(slot) in
    player.x <- x;
    player.y <- y;
    player.facing_x <- Match.direction player.team;
    player.facing_y <- 0.

let test_solo_switch () =
    let client = new_client (peer 's') in
    let session = client_session client in
    let game = Session.game session in
    game.phase <- Match.Playing;
    game.pause <- 0;
    place game 3 56. 8.;
    place game 13 58. 8.;
    Match.take_ball game.ball game.players.(13) 0;
    let moving = { Match.idle with dx = 1. } in
    Session.frame session Session.step_time { moving with pass = true };
    assert (Session.local_slot session = Some 3);
    assert (session.inputs.(6) = None && session.inputs.(3) = Some moving);
    assert (game.players.(3).x > 56. && game.ball.owner = Some 13);

    (* The same field and chosen shirt carry over when practice goes live. *)
    membership [];
    start client;
    membership [client];
    assert (Session.status session = Session.Online);
    assert (Session.game session == game && Session.local_slot session = Some 3);
    let x = game.players.(3).x in
    Session.frame session Session.step_time moving;
    assert (game.players.(3).x > x && session.inputs.(6) = None)

let test_multiplayer_switch () =
    membership [];
    let host = new_client (peer 'a') and runner = new_client (peer 'b') in
    let opponent = new_client (peer 'c') and teammate = new_client (peer 'd') in
    let authority = client_session host and follower = client_session runner in
    List.iter start [host; runner; opponent; teammate];
    membership [host; runner; opponent; teammate];
    frames 45;
    assert (Session.local_slot follower = Some 13);
    assert (Session.local_slot (client_session teammate) = Some 11);
    let game = Session.game authority in
    game.phase <- Match.Playing;
    game.pause <- 0;
    for slot = 0 to 6 do place game slot (8. +. float slot *. 3.) 22. done;
    for slot = 7 to 12 do place game slot (76. +. float (slot - 7) *. 3.) 52. done;
    place game 13 66. 32.;
    place game 10 51. 32.;
    Match.take_ball game.ball game.players.(13) 0;
    blocked_state_for := Some (fst runner);

    let moving = { Match.idle with dx = -1. } in
    frame (fun id -> if id = fst runner then { moving with pass = true } else Match.idle);
    for _ = 1 to 22 do frame (fun id -> if id = fst runner then moving else Match.idle) done;
    assert (game.ball.owner = Some 10);
    assert ((occupied authority).slots.(10) = Some (fst runner));
    assert ((occupied authority).slots.(11) = Some (fst teammate));
    assert ((occupied authority).slots.(13) = None && authority.inputs.(13) = None);

    (* A follower can still be looking at the previous shirt. Its held direction
       reaches the new one, and a shot addressed to the old one expires. *)
    assert (Session.local_slot follower = Some 13);
    let downward = { Match.idle with dy = 1. } in
    let new_y = game.players.(10).y and old_x = game.players.(13).x in
    frame (fun id -> if id = fst runner then { downward with shoot = true } else Match.idle);
    for _ = 1 to 5 do frame (fun id -> if id = fst runner then downward else Match.idle) done;
    assert (game.players.(10).y > new_y +. 1. && game.ball.owner = Some 10);
    assert (game.players.(13).x <> old_x);

    (* An action still waiting to be sent must also expire on the roster change. *)
    Playbit.current := fst runner;
    Playbit.publish_error := Some Playbit.Error.limit_exceeded;
    Session.frame follower 0.06 { downward with shoot = true };
    Playbit.publish_error := None;
    assert (follower.input.shoot);
    blocked_state_for := None;
    for _ = 1 to 12 do frame (fun id -> if id = fst runner then downward else Match.idle) done;
    same_roster authority follower;
    assert (Session.local_slot follower = Some 10);
    assert (game.ball.owner = Some 10 && not follower.input.shoot);
    assert (follower.input.dy = downward.dy)

let () =
    let a = new_client (peer 'm') in
    let b = new_client (peer 't') in
    let c = new_client (peer 'z') in
    let d = new_client (peer 'a') in
    let ca, cb, cc, cd = client_session a, client_session b, client_session c, client_session d in

    Playbit.blocked := true;
    start a;
    membership [a];
    assert (Session.status ca = Session.Offline);
    frames 60;
    let offline_tick = tick ca in
    assert (offline_tick > 20);
    Playbit.blocked := false;
    membership [a];
    assert (Session.status ca = Session.Online);
    assert (tick ca = offline_tick);

    (* Concurrent arrivals preserve the existing slot and balance teams. *)
    start b;
    start c;
    membership [c; a; b];
    frames 30;
    same_roster ca cb;
    same_roster ca cc;
    assert (Session.local_slot ca = Some 6);
    assert (Session.local_slot cb = Some 13);
    assert (Session.local_slot cc = Some 4);

    (* A newcomer with the lowest identity takes over the existing game. *)
    let previous_tick = tick ca in
    start d;
    membership [d; a; b; c];
    frames 60;
    assert (Session.is_authority cd && not (Session.is_authority ca));
    assert (tick cd > previous_tick);
    same_roster cd cb;
    assert (Session.local_slot ca = Some 6 && Session.local_slot cb = Some 13);
    assert (Session.local_slot cd = Some 11);

    (* A follower's first frame moves its own display player before the next
       snapshot. Match physics remain unchanged until the authority applies input. *)
    let defender = Option.get (Session.local_slot cb) in
    let before, _ = Session.display_position cb defender in
    let moving = { Match.idle with dx = -1. } in
    Playbit.current := fst b;
    Session.frame cb (1. /. 60.) moving;
    assert (fst (Session.display_position cb defender) < before);
    assert ((Session.game cb).players.(defender).x = before);
    for _ = 1 to 40 do frame (fun id -> if id = fst b then moving else Match.idle) done;
    assert ((Session.game cd).players.(defender).x < before -. 4.);

    (* An authenticated non-authority cannot advance the round by sending state. *)
    let forged = { (occupied cc) with round = 42 } in
    process b { kind = 18l; data = Message ("touchline-v1", fst c,
        Wire.encode (Wire.State forged)) };
    assert ((occupied cb).round = 1);

    (* The authority and another player depart concurrently. Their old slots
       immediately become CPU players once the successor continues the match. *)
    let previous_tick = tick cb in
    membership [a; b];
    frames 60;
    assert (Session.is_authority ca);
    assert (tick ca > previous_tick);
    same_roster ca cb;
    assert (Session.human_count ca = 2);
    assert (ca.inputs.(11) = None && ca.inputs.(4) = None);

    (* Returning lowest identity must import the newer snapshot. *)
    let previous_tick = tick ca in
    membership [a; b; d];
    frames 60;
    assert (Session.is_authority cd && tick cd > previous_tick);
    same_roster ca cd;

    let extra = List.init 12 (fun i -> new_client (peer (Char.chr (200 + i)))) in
    List.iter start extra;
    membership (a :: b :: d :: extra);
    frames 30;
    assert (Session.human_count cd = 14);
    let excluded = List.filter
        (fun (_, session) -> Session.status session = Session.Full) extra in
    assert (List.length excluded = 1);
    let waiting = List.hd excluded in
    membership (b :: d :: extra);
    frames 30;
    assert (Session.status (client_session waiting) = Session.Online);
    assert (Session.human_count cd = 14);

    (* A fresh lowest identity waits for a positive handoff even when packets
       arrive well after the ordinary election settling delay. *)
    let entrant = new_client (peer 'A') in
    let full_room = b :: d :: extra in
    let previous_seats = Array.copy (occupied cd).slots in
    let previous_tick = tick cd in
    blocked_offers_for := Some (fst entrant);
    start entrant;
    membership (entrant :: full_room);
    frames 120;
    assert (Session.status (client_session entrant) = Session.Connecting);
    assert ((client_session entrant).live = None);
    blocked_offers_for := None;
    frames 120;
    assert (Session.status (client_session entrant) = Session.Full);
    assert ((occupied (client_session entrant)).slots = previous_seats);
    assert (tick (client_session entrant) > previous_tick);
    membership full_room;
    frames 60;

    process b { kind = 19l; data = Status (Int32.of_int Playbit.Error.limit_exceeded) };
    flush ();
    frames 15;
    assert (Session.status cb = Session.Online);

    process b { kind = 19l; data = Status (Int32.of_int Playbit.Error.unavailable) };
    assert (Session.status cb = Session.Offline);
    start b;
    frames 15;
    assert (Session.status cb = Session.Online);

    (Session.game cd).phase <- Match.Full_time;
    (Session.game cd).ticks_left <- 0;
    cd.dirty <- true;
    frames 4;
    let late = new_client (peer 'l') in
    blocked_state_for := Some (fst late);
    start late;
    membership [b; d; late];
    frames 60;
    assert (Session.status (client_session late) = Session.Connecting);
    blocked_state_for := None;
    frames 120;
    assert (Session.status (client_session late) = Session.Online);
    assert ((Session.game (client_session late)).phase = Match.Full_time);

    let stale = Wire.encode (Wire.State (occupied cd)) in
    cd.inputs.(defender) <- Some moving;
    cd.input_ttl.(defender) <- 0.75;
    Playbit.publish_error := Some Playbit.Error.unavailable;
    Playbit.current := fst b;
    Session.restart cb;
    flush ();
    assert (Session.status cb = Session.Online && (occupied cb).round = 1);
    Playbit.publish_error := None;
    Playbit.current := fst d;
    frames 15;
    assert (cd.inputs.(defender) = Some Match.idle && cd.input_ttl.(defender) = 0.);
    process b { kind = 18l; data = Message ("touchline-v1", fst d, stale) };
    assert ((occupied cb).round = 2);

    let valid = Wire.encode (Wire.State (occupied cd)) in
    assert (Option.is_some (Wire.decode valid));
    assert (Wire.decode (String.sub valid 0 (String.length valid - 1)) = None);
    let duplicated = Bytes.of_string valid in
    Bytes.blit duplicated 8 duplicated (8 + Wire.peer_bytes) Wire.peer_bytes;
    assert (Wire.decode (Bytes.unsafe_to_string duplicated) = None);

    let fresh_a = new_client (peer '1') and fresh_b = new_client (peer '2') in
    start fresh_a;
    start fresh_b;
    membership [fresh_a; fresh_b];
    frames 60;
    assert (Session.status (client_session fresh_a) = Session.Online);
    same_roster (client_session fresh_a) (client_session fresh_b);

    test_solo_switch ();
    test_multiplayer_switch ();
    print_endline "Touchline session tests passed"
