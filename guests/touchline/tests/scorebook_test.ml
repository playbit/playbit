let finish ~writable =
    Playbit.Data.writable := writable;
    Playbit.Data.writes := [];
    let book = Scorebook.create () in
    let joined = ref false in
    let join () = joined := true in
    Scorebook.open_session book join;
    Scorebook.complete book join;
    assert !joined;
    let game = Match.create () in
    game.away_score <- 1;
    Scorebook.full_time book game;
    Scorebook.complete book join;
    assert (book.notice = None);
    !Playbit.Data.writes

let () =
    assert (finish ~writable:false = []);
    assert (finish ~writable:true = ["scorebook", "last-result", "Portside FC 0 – 1 Rovers"]);
    print_endline "Touchline scorebook tests passed."
