use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Worker count for CPU stages: `UNPACK_THREADS` if set, else the core count capped at 8
pub fn threads() -> usize {
    if let Some(n) = std::env::var("UNPACK_THREADS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
    {
        return n.max(1);
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8)
}

/// Run `f` over `items` on `threads` scoped workers that pull the next index from a shared counter
/// `init` builds per-worker state (an open zip, a buffer). The first error stops the other workers
/// from taking new items; a panic in `f` is reported as an error instead of unwinding the caller
pub fn for_each<T, S, I, F>(items: &[T], threads: usize, init: I, f: F) -> Result<(), String>
where
    T: Sync,
    I: Fn() -> Result<S, String> + Sync,
    F: Fn(&mut S, usize, &T) -> Result<(), String> + Sync,
{
    let next = AtomicUsize::new(0);
    let error: Mutex<Option<String>> = Mutex::new(None);
    let fail = |err: String| {
        let mut slot = error.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert(err);
    };
    let failed = || error.lock().map(|g| g.is_some()).unwrap_or(true);
    let workers = threads.clamp(1, items.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut state = match init() {
                    Ok(state) => state,
                    Err(err) => return fail(err),
                };
                while !failed() {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    match catch_unwind(AssertUnwindSafe(|| f(&mut state, i, item))) {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => fail(err),
                        Err(panic) => fail(format!("worker panicked: {}", panic_message(&*panic))),
                    }
                }
            });
        }
    });
    match error.into_inner().unwrap_or_else(|e| e.into_inner()) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visits_every_item_once_and_reports_the_first_error() {
        let items: Vec<usize> = (0..1000).collect();
        let seen = Mutex::new(vec![0u8; items.len()]);
        for_each(
            &items,
            6,
            || Ok(()),
            |_, i, item| {
                assert_eq!(i, *item);
                seen.lock().unwrap()[i] += 1;
                Ok(())
            },
        )
        .unwrap();
        assert!(seen.into_inner().unwrap().iter().all(|n| *n == 1));
        let err = for_each(
            &items,
            4,
            || Ok(()),
            |_, i, _| if i == 500 { Err("boom".into()) } else { Ok(()) },
        );
        assert_eq!(err.unwrap_err(), "boom");
        let err = for_each(
            &items,
            4,
            || Ok(()),
            |_, i, _| if i == 7 { panic!("bad item") } else { Ok(()) },
        );
        assert!(err.unwrap_err().contains("bad item"));
    }
}
