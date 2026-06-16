// build.rs — generate versioned service worker
//
// Hashes assets/index.html and bakes the short hash into the service
// worker cache name so that any HTML change invalidates the SW cache.
// The generated sw.js is written to OUT_DIR and included at compile time.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

fn main() {
    // Re-run if either source file changes.
    println!("cargo:rerun-if-changed=assets/index.html");
    println!("cargo:rerun-if-changed=assets/sw.js");

    let index = std::fs::read_to_string("assets/index.html").expect("assets/index.html not found");

    let mut hasher = DefaultHasher::new();
    index.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());

    // Short prefix is enough — collisions don't matter, we just need
    // the key to change when the HTML changes.
    let cache_key = format!("goop-{}", &hash[..8]);

    let sw = std::fs::read_to_string("assets/sw.js").expect("assets/sw.js not found");
    let sw = sw.replace("\"goop-v2\"", &format!("\"{}\"", cache_key));

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    std::fs::write(format!("{out_dir}/sw.js"), sw).expect("failed to write generated sw.js");
}
