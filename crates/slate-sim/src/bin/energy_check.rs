use slate_sim::power::{report, PowerModel, Stats};

fn simulate_workload(b: u64) -> Stats {
    // A simplified model of the workload over e.g. 1000 ops
    let ops = 1000;

    // Wakes happen every B ops
    let wakes = ops / b;

    // Each commit (wake) writes roughly O(1) header + batch
    let ckpt_bytes = wakes * 256;
    let user_bytes = ops * 64; // ~64 bytes per op
    let gc_bytes = ops * 10; // GC overhead estimate
    let parity_bytes = user_bytes / 4; // u=0.5, k=8, m=4

    let total_bytes = user_bytes + gc_bytes + parity_bytes + ckpt_bytes;
    let erases = total_bytes / 4096; // 4KB blocks

    Stats {
        commits: wakes,
        wakes,
        user_bytes,
        gc_bytes,
        parity_bytes,
        ckpt_bytes,
        erases,
    }
}

fn main() {
    let bs = [3, 9, 27, 81];
    let model = PowerModel::default();

    println!("B,mJ_per_op,label");
    for &b in &bs {
        let stats = simulate_workload(b);
        let rep = report(&stats, &model);
        let mj_per_op = rep.m_joules / 1000.0;
        println!("{},{},{}", b, mj_per_op, rep.label);
    }
}
