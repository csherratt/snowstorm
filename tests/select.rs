
extern crate snowstorm;

use std::thread;
use snowstorm::select::*;

#[test]
fn simple_select() {
    let mut select = Select::new();
    let h1 = select.handle();
    let h1_id = h1.id();
    let h2 = select.handle();
    let h2_id = h2.id();

    assert!(select.ready().is_none());
    h1.trigger();
    assert_eq!(select.ready().map(|x| x.id()), Some(h1_id));
    h2.trigger();
    assert_eq!(select.ready().map(|x| x.id()), Some(h2_id));
    assert!(select.ready().is_none());
}

#[test]
fn simple_select_reuse() {
    let mut select = Select::new();
    let h1 = select.handle();
    let h1_id = h1.id();
    let h2 = select.handle();
    let h2_id = h2.id();

    assert!(select.ready().is_none());
    h1.trigger();
    for _ in 0..1000 {
        select.ready().map(|x| {
            assert_eq!(x.id(), h1_id);
            x.trigger();
        });
    }
    select.ready();
    assert!(select.ready().is_none());
    h2.trigger();
    for _ in 0..1000 {
        select.ready().map(|x| {
            assert_eq!(x.id(), h2_id);
            x.trigger();
        });
    }
}

#[test]
fn collect_1000() {
    let select = Select::new();
    let handles: Vec<Handle> = (0..1000).map(|_| select.handle()).collect();
    thread::spawn(move || {
        for handle in handles {
            handle.trigger();
            thread::sleep_ms(0);
        }
    });

    let mut count = 0;
    for _ in select {
        count += 1;
        if count == 1000 {
            break;
        }
    }
}