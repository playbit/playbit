//! Fuzz two replicas: arbitrary interleaved edits on both sides, then
//! bidirectional sync must converge to identical roots and contents,
//! with sessions completing (no stall, no error, no panic).
#![no_main]

mod support;

use arbitrary::Arbitrary;
use kvdb::Lww;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Plan {
    ops: Vec<Step>,
}

#[derive(Arbitrary, Debug)]
enum Step {
    Put {
        on_b: bool,
        k: u8,
        v: u8,
        big: bool,
    },
    Del {
        on_b: bool,
        k: u8,
    },
    Forget {
        on_b: bool,
        k: u8,
    },
    /// Pull changes: a←b or b←a.
    Sync {
        into_a: bool,
    },
}

fn key(k: u8) -> Vec<u8> {
    vec![b'a' + (k % 12); 1 + ((k >> 4) % 4) as usize]
}

fuzz_target!(|plan: Plan| {
    let (a, b) = (support::mk_db(4), support::mk_db(4));
    let mut wall = support::WALL;

    let mut ops = plan.ops;
    ops.truncate(64);
    for step in ops {
        wall += 1;
        let db = |on_b: bool| if on_b { &b } else { &a };
        match step {
            Step::Put { on_b, k, v, big } => {
                let value = if big { vec![v; 40] } else { vec![v; (v % 24) as usize] };
                let mut w = db(on_b).begin_write(wall).unwrap();
                w.put(&key(k), &value).unwrap();
                w.verify().unwrap();
                w.commit().unwrap();
            }
            Step::Del { on_b, k } => {
                let mut w = db(on_b).begin_write(wall).unwrap();
                let _ = w.del(&key(k)).unwrap();
                w.verify().unwrap();
                w.commit().unwrap();
            }
            Step::Forget { on_b, k } => {
                let mut w = db(on_b).begin_write(wall).unwrap();
                let _ = w.del_forget(&key(k)).unwrap();
                w.verify().unwrap();
                w.commit().unwrap();
            }
            Step::Sync { into_a } => {
                if into_a {
                    support::pump(&a, &b, Lww);
                } else {
                    support::pump(&b, &a, Lww);
                }
            }
        }
        a.verify().unwrap();
        b.verify().unwrap();
    }

    // Full bidirectional quiesce must converge the replicas.
    support::assert_convergence(&a, &b);
});
