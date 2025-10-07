use std::{collections::{BTreeSet, HashSet}, time::Duration};

use nohash_hasher::IntSet;
use tokio::time::{sleep, Instant};

#[tokio::main]
pub async fn main() {
    let mut set = IntSet::default();

    for i in 0..20_000_000 {
        set.insert(i);
    }

    println!("Finished Insertion");

    let now = Instant::now();
    let mut k = 0;
    for i in 0..20_000_000 {
        if let Some(val) = set.get(&(i + 1)) {
            k += val | 22 ;
        } else {
            k -= 1;
        }
    }
    println!("{:?} {k}", now.elapsed());

    sleep(Duration::from_secs(30)).await;
    for i in 0..20_000_000 {
        if let Some(val) = set.get(&(i + 1)) {
            k += val | 22 ;
        } else {
            k -= 1;
        }
    }
    println!("{:?} {k}", now.elapsed());

}

// INT SET:
