#![allow(dead_code)]
mod data {
    #[path = "@EC_PATH@"]
    mod ec;
    use ec::{Codec, DecodeWorkspace, Kernel, ShardMask};
    use std::{
        hint::black_box,
        time::{Duration, Instant},
    };

    fn speed(mut f: impl FnMut(), bytes: usize) -> f64 {
        let mut samples = [0.0; 3];
        for sample in &mut samples {
            let start = Instant::now();
            let mut count = 0;
            loop {
                for _ in 0..4 {
                    f();
                }
                count += 4;
                if start.elapsed() >= Duration::from_millis(45) {
                    break;
                }
            }
            *sample =
                count as f64 * bytes as f64 / start.elapsed().as_secs_f64() / (1u64 << 30) as f64;
        }
        samples.sort_by(f64::total_cmp);
        samples[1]
    }

    #[test]
    #[ignore]
    fn compare() {
        let names: Vec<_> = Kernel::supported().map(|k| k.name()).collect();
        println!("kernels={names:?}, selected={}", Kernel::detect().name());
        println!("geometry,bytes,operation,backend,GiB/s");
        for (k, m) in [(2, 1), (4, 2), (8, 4), (16, 8)] {
            for len in [4096, 65536, 1048576] {
                let old = reed_solomon_engine::Codec::new(k, m).unwrap();
                let mut original: Vec<_> = (0..k + m)
                    .map(|i| {
                        (0..len)
                            .map(|j| ((i * 7 + j * 11) & 255) as u8)
                            .collect::<Vec<_>>()
                    })
                    .collect();
                old.encode_all(&mut original).unwrap();
                let mut shards = original.clone();
                let enc = speed(|| old.encode_all(black_box(&mut shards)).unwrap(), k * len);
                println!(
                    "{k}+{m},{len},encode,vendor-{},{enc:.3}",
                    old.kernel().name()
                );
                let mut old_mask = reed_solomon_engine::ShardMask::all_present(k + m);
                old_mask.remove(0);
                let mut old_ws = old.workspace();
                let dec = speed(
                    || {
                        old.reconstruct_data(black_box(&mut shards), &old_mask, &mut old_ws)
                            .unwrap()
                    },
                    len,
                );
                println!(
                    "{k}+{m},{len},decode,vendor-{},{dec:.3}",
                    old.kernel().name()
                );
                let mut mask = ShardMask::all_present(k + m);
                mask.remove(0);
                for kernel in
                    Kernel::supported().filter(|k| k.name() != "scalar" && k.name() != "ssse3")
                {
                    let native = Codec::with_kernel(kernel, k, m).unwrap();
                    let mut shards = original.clone();
                    native.encode_all(&mut shards).unwrap();
                    assert_eq!(shards, original);
                    let enc = speed(
                        || native.encode_all(black_box(&mut shards)).unwrap(),
                        k * len,
                    );
                    println!("{k}+{m},{len},encode,{},{enc:.3}", kernel.name());
                    let mut ws = DecodeWorkspace::new();
                    shards[0].fill(0);
                    native
                        .reconstruct_data(&mut shards, &mask, &mut ws)
                        .unwrap();
                    assert_eq!(shards, original);
                    let dec = speed(
                        || {
                            native
                                .reconstruct_data(black_box(&mut shards), &mask, &mut ws)
                                .unwrap()
                        },
                        len,
                    );
                    println!("{k}+{m},{len},decode,{},{dec:.3}", kernel.name());
                }
            }
        }
    }
}
