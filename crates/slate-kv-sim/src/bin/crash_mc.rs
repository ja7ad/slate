use rand::{rngs::StdRng, Rng, SeedableRng};
use slate_kv_sim::sim_db::{Db, KeySource, Options};
use slate_kv_sim::{Crash, SimCounter, SimFlash};

fn run_crash_mc(trials: usize, stale_trials: usize) {
    let mut rng = StdRng::seed_from_u64(42);
    let mut violations = 0;

    let root_key = [0x42; 32];
    let opts = Options {
        capacity: 1024 * 1024, // 1MB
        b_commit: 8,
        auto_b: false,
        n_keys: 1024,
        ..Default::default()
    };

    println!("Running {} crash trials...", trials);
    for trial in 0..trials {
        let trial_seed = rng.gen::<u64>();

        // --- Pass 1: Count total ops ---
        let mut count_rng = StdRng::seed_from_u64(trial_seed);
        let flash = SimFlash::new(opts.capacity, 256, 4096);
        let counter = SimCounter::new(100_000);
        let mut db = Db::open(KeySource::Bytes(root_key), opts.clone(), flash, counter).unwrap();
        let start_ops = db.flash_mut(|f| f.power.current_op);

        let mut ground_truth = Vec::new();
        for _ in 0..600 {
            let k_len = count_rng.gen_range(16..32);
            let v_len = count_rng.gen_range(1..128);
            let mut key = vec![0u8; k_len];
            let mut val = vec![0u8; v_len];
            count_rng.fill(&mut key[..]);
            count_rng.fill(&mut val[..]);

            ground_truth.push((key.clone(), val.clone()));
            db.put(&key, &val).unwrap();
        }

        let (flash, _) = db.take_flash_and_counter();
        let total_ops = flash.power.current_op;

        // --- Pass 2: Crash ---
        let crash_op = rng.gen_range(start_ops..total_ops);
        let crash_byte = rng.gen_range(0..256);

        let crash_flash = SimFlash::new(opts.capacity, 256, 4096);
        let crash_counter = SimCounter::new(100_000);

        // Mount without crash trigger
        let mut db = Db::open(
            KeySource::Bytes(root_key),
            opts.clone(),
            crash_flash,
            crash_counter,
        )
        .unwrap();

        // Enable crash trigger
        db.flash_mut(|f| {
            f.power.crash = Crash::AtByte {
                op_index: crash_op,
                byte_in_op: crash_byte,
            };
        });

        for (key, val) in &ground_truth {
            let res = db.put(key, val);
            if res.is_err() {
                break;
            }
        }
        let (mut final_flash, final_counter) = db.take_flash_and_counter();

        // --- Pass 3: Recover ---
        final_flash.power.crash = Crash::None;
        let rec_db = Db::open(
            KeySource::Bytes(root_key),
            opts.clone(),
            final_flash,
            final_counter,
        )
        .expect("Recovery failed");

        let acked = rec_db.acked_seq() as usize;
        let _epoch = rec_db.epoch();

        // Verify acknowledged prefix
        // The acknowledged ground truth is exactly `ground_truth[..acked]`
        for (i, (k, v)) in ground_truth.iter().enumerate().take(acked) {
            let rec_val = rec_db.get(k).unwrap().expect("Missing acked key");
            if rec_val != *v {
                println!("Trial {}: value mismatch at seq {}", trial, i + 1);
                violations += 1;
            }
        }

        // Check for torn/uncommitted acceptance: length of DB should not exceed acked.
        if rec_db.len() > acked {
            println!(
                "Trial {}: torn acceptance! DB length {} > acked {}",
                trial,
                rec_db.len(),
                acked
            );
            violations += 1;
        }
    }

    if violations > 0 {
        panic!("Found {} violations!", violations);
    }
    println!("Crash MC passed! 0 violations.");

    println!("Running {} stale-epoch trials...", stale_trials);
    for trial in 0..stale_trials {
        let trial_seed = rng.gen::<u64>();
        let mut count_rng = StdRng::seed_from_u64(trial_seed);
        let flash = SimFlash::new(opts.capacity, 256, 4096);
        let counter = SimCounter::new(100_000);
        let mut db = Db::open(KeySource::Bytes(root_key), opts.clone(), flash, counter).unwrap();

        // Write 32 records (B=8, so this takes 4 epochs)
        for _ in 0..32 {
            let k_len = count_rng.gen_range(16..32);
            let v_len = count_rng.gen_range(1..128);
            let mut key = vec![0u8; k_len];
            let mut val = vec![0u8; v_len];
            count_rng.fill(&mut key[..]);
            count_rng.fill(&mut val[..]);
            db.put(&key, &val).unwrap();
        }

        let (mut final_flash, final_counter) = db.take_flash_and_counter();

        // Find the tail of the log (first page that is all 0xFF)
        let page_size = 256;
        let mut tail_page_idx = 0;
        let mut first_data_page = 0;
        let data_base = slate_kv_core::config::data_base_offset(4096) as usize;

        for i in (data_base / page_size)..(final_flash.mem.len() / page_size) {
            let offset = i * page_size;
            let page = &final_flash.mem[offset..offset + page_size];
            if !page.iter().all(|&b| b == 0xFF) {
                if first_data_page == 0 {
                    first_data_page = i;
                }
            } else if first_data_page != 0 {
                tail_page_idx = i;
                break;
            }
        }

        if tail_page_idx == 0 || first_data_page == 0 {
            panic!("Could not find data or tail in stale-epoch trial");
        }

        // Copy the first data page (epoch 1) to the tail (which belongs to a later epoch)
        let stale_page = final_flash.mem
            [first_data_page * page_size..first_data_page * page_size + page_size]
            .to_vec();
        let tail_offset = tail_page_idx * page_size;
        for (i, &b) in stale_page.iter().enumerate() {
            final_flash.mem[tail_offset + i] &= b;
        }

        // Recover
        let rec_db = Db::open(
            KeySource::Bytes(root_key),
            opts.clone(),
            final_flash,
            final_counter,
        )
        .expect("Recovery failed");

        // Acked should STILL be 32, because the stale records were rejected!
        if rec_db.acked_seq() != 32 {
            println!(
                "Trial {}: Stale records accepted! Acked = {}",
                trial,
                rec_db.acked_seq()
            );
            violations += 1;
        }
    }

    if violations > 0 {
        panic!("Found {} stale-epoch violations!", violations);
    }
    println!("Stale-epoch MC passed! 100% rejected.");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let trials = if args.len() > 1 && args[1] == "--pr" {
        2000
    } else {
        20000
    };
    let stale_trials = if trials == 2000 { 500 } else { 5000 };
    run_crash_mc(trials, stale_trials);
}
